// Native half of `spherekit-jsengine`.
//
// The contract with Rust is deliberately narrow: V8 handles never leave a
// function in this file, and the only things that cross the FFI boundary are
// owned UTF-8 byte buffers and plain integers. That is what lets the Rust side
// be safe without modelling `v8::Local<T>`'s stack discipline in the type
// system.
//
// Ownership rules, in one place:
//   * every `uint8_t**` out-parameter is filled with a buffer allocated by
//     `std::malloc`; the receiver releases it with `spherekit_v8_free_buffer`.
//   * `spherekit_v8_error` must be passed in zeroed. On a non-zero status its
//     buffers are owned by the caller, on `kOk` they stay null.
//   * a host callback's `result`/`error` buffers come from `spherekit_v8_alloc`
//     and are freed here, by the same allocator, as soon as they are consumed.
//   * a `spherekit_v8_host_callback`'s `user_data` is owned by Rust for the
//     whole lifetime of the engine; this file only stores and passes it back.

#include <v8.h>
#include <libplatform/libplatform.h>

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <memory>
#include <mutex>
#include <new>
#include <string>
#include <vector>

// Detail carried out of a failed call. Kept as a struct rather than the older
// fixed `char[]` because an exception has a position and a stack as well as a
// message, and truncating any of them into one buffer loses exactly the part a
// developer needs.
struct spherekit_v8_error {
  uint8_t* message;
  size_t message_length;
  uint8_t* stack;
  size_t stack_length;
  uint8_t* resource;
  size_t resource_length;
  uint32_t line;
  uint32_t column;
};

// A Rust host function seen from C++. Returns 0 when `result` holds the return
// value, non-zero when `error` holds a message to throw. Both buffers are
// allocated by `spherekit_v8_alloc` and freed here.
typedef int32_t (*spherekit_v8_host_callback)(void* user_data,
                                              const uint8_t* argument,
                                              size_t argument_length,
                                              uint8_t** result,
                                              size_t* result_length,
                                              uint8_t** error,
                                              size_t* error_length);

namespace {

constexpr int kOk = 0;
constexpr int kInvalidArgument = 1;
constexpr int kInitializationFailed = 2;
constexpr int kOutOfMemory = 3;
constexpr int kJavaScriptError = 4;
constexpr int kInvalidBinding = 5;

// `pump` runs on the frame thread. Draining without a bound would let a task
// that reposts itself starve rendering forever, so a stuck queue costs one
// frame of latency instead of the whole UI.
constexpr int kMaxPumpedTasks = 1024;

// V8 tags external pointers so a `void*` stored as one C++ type cannot be read
// back as another. This shim stores exactly one kind of external — a binding
// record — and giving it its own tag rather than the default means any other
// external that ever reaches the callback fails the read instead of being
// reinterpreted as a `HostFunctionRecord*`.
constexpr v8::ExternalPointerTypeTag kHostFunctionTag = 1;

std::once_flag g_v8_once;
// V8's platform must outlive all isolates and the V8 runtime. Intentionally
// leak it for the process lifetime so static teardown cannot destroy it after
// V8's own global state has already started shutting down.
v8::Platform* g_platform = nullptr;
std::string g_v8_initialization_error;

struct HostFunctionRecord {
  spherekit_v8_host_callback callback = nullptr;
  void* user_data = nullptr;
};

bool copy_bytes(const char* data, size_t length, uint8_t** out, size_t* out_length) {
  // A zero-length allocation still returns a distinct pointer so the Rust side
  // never has to treat "empty" and "absent" as the same thing.
  auto* bytes = static_cast<uint8_t*>(std::malloc(length == 0 ? 1 : length));
  if (bytes == nullptr) return false;
  if (length != 0) std::memcpy(bytes, data, length);
  *out = bytes;
  *out_length = length;
  return true;
}

void reset_error(spherekit_v8_error* error) {
  if (error == nullptr) return;
  *error = spherekit_v8_error{};
}

int fail(spherekit_v8_error* error, int status, const char* message) {
  const char* text = message == nullptr ? "V8 operation failed" : message;
  if (error != nullptr) {
    copy_bytes(text, std::strlen(text), &error->message, &error->message_length);
  }
  return status;
}

bool to_v8_string(v8::Isolate* isolate,
                  const uint8_t* data,
                  size_t length,
                  v8::Local<v8::String>* out) {
  if (length > static_cast<size_t>(std::numeric_limits<int>::max())) return false;
  const char* bytes = data == nullptr ? "" : reinterpret_cast<const char*>(data);
  return v8::String::NewFromUtf8(isolate, bytes, v8::NewStringType::kNormal,
                                 static_cast<int>(length))
      .ToLocal(out);
}

void store_utf8(v8::Isolate* isolate,
                v8::Local<v8::Value> value,
                uint8_t** out,
                size_t* out_length) {
  if (value.IsEmpty() || value->IsUndefined() || value->IsNull()) return;
  v8::String::Utf8Value text(isolate, value);
  if (*text == nullptr) return;
  copy_bytes(*text, static_cast<size_t>(text.length()), out, out_length);
}

// Turns a caught exception into the full error record. Position and stack come
// from two different V8 objects — `Message` knows where, the exception's own
// `stack` property knows how it got there — so both are read here rather than
// making every caller remember to.
int report_exception(spherekit_v8_error* error,
                     v8::Isolate* isolate,
                     v8::Local<v8::Context> context,
                     v8::TryCatch& try_catch) {
  if (error == nullptr) return kJavaScriptError;

  std::string message = "JavaScript exception";
  {
    v8::String::Utf8Value text(isolate, try_catch.Exception());
    if (*text != nullptr && text.length() != 0) {
      message.assign(*text, static_cast<size_t>(text.length()));
    }
  }
  copy_bytes(message.data(), message.size(), &error->message, &error->message_length);

  v8::Local<v8::Value> stack;
  if (try_catch.StackTrace(context).ToLocal(&stack)) {
    store_utf8(isolate, stack, &error->stack, &error->stack_length);
  }

  v8::Local<v8::Message> detail = try_catch.Message();
  if (!detail.IsEmpty()) {
    store_utf8(isolate, detail->GetScriptResourceName(), &error->resource, &error->resource_length);
    int line = 0;
    if (detail->GetLineNumber(context).To(&line) && line > 0) {
      error->line = static_cast<uint32_t>(line);
    }
    const int column = detail->GetStartColumn();
    if (column >= 0) {
      // V8 counts columns from zero and lines from one. Both are reported
      // one-based here so `resource:line:column` reads like every other
      // compiler's diagnostic instead of being off by one in the middle.
      error->column = static_cast<uint32_t>(column) + 1;
    }
  }
  return kJavaScriptError;
}

bool initialize_v8(const char* icu_data_path) {
  std::call_once(g_v8_once, [icu_data_path] {
    if (!v8::V8::InitializeICUDefaultLocation(nullptr, icu_data_path)) {
      g_v8_initialization_error = "V8 ICU initialization failed";
      return;
    }

    auto platform = v8::platform::NewDefaultPlatform();
    if (!platform) {
      g_v8_initialization_error = "V8 platform creation failed";
      return;
    }

    g_platform = platform.release();
    v8::V8::InitializePlatform(g_platform);
    if (!v8::V8::Initialize()) {
      g_v8_initialization_error = "V8 initialization failed";
    }
  });
  return g_v8_initialization_error.empty();
}

void throw_host_error(v8::Isolate* isolate, const std::string& message) {
  v8::Local<v8::String> text;
  if (!to_v8_string(isolate, reinterpret_cast<const uint8_t*>(message.data()), message.size(),
                    &text)) {
    text = v8::String::Empty(isolate);
  }
  isolate->ThrowException(v8::Exception::Error(text));
}

// The single entry point every bound host function goes through. The trampoline
// record lives in the `FunctionTemplate`'s data as a `v8::External`, which is
// what keeps this callback stateless and lets one C++ function serve every
// binding.
void host_function_callback(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Value> data = info.Data();
  if (data.IsEmpty() || !data->IsExternal()) {
    throw_host_error(isolate, "host function is missing its binding record");
    return;
  }
  auto* record =
      static_cast<HostFunctionRecord*>(data.As<v8::External>()->Value(kHostFunctionTag));
  if (record == nullptr || record->callback == nullptr) {
    throw_host_error(isolate, "host function is missing its binding record");
    return;
  }

  std::string argument;
  if (info.Length() > 0) {
    v8::TryCatch try_catch(isolate);
    v8::String::Utf8Value text(isolate, info[0]);
    if (*text == nullptr) {
      // A throwing `toString` is the caller's bug, not the host's; rethrowing
      // keeps the original error rather than replacing it with a vague one.
      if (try_catch.HasCaught()) {
        try_catch.ReThrow();
        return;
      }
      throw_host_error(isolate, "host function argument is not convertible to a string");
      return;
    }
    argument.assign(*text, static_cast<size_t>(text.length()));
  }

  uint8_t* result = nullptr;
  size_t result_length = 0;
  uint8_t* error = nullptr;
  size_t error_length = 0;
  const int32_t status = record->callback(record->user_data,
                                          reinterpret_cast<const uint8_t*>(argument.data()),
                                          argument.size(), &result, &result_length, &error,
                                          &error_length);

  if (status != 0) {
    std::string message = "host function failed";
    if (error != nullptr) message.assign(reinterpret_cast<char*>(error), error_length);
    std::free(error);
    std::free(result);
    throw_host_error(isolate, message);
    return;
  }

  std::free(error);
  v8::Local<v8::String> value;
  const bool converted = to_v8_string(isolate, result, result_length, &value);
  std::free(result);
  if (!converted) {
    throw_host_error(isolate, "host function result is not valid UTF-8");
    return;
  }
  info.GetReturnValue().Set(value);
}

}  // namespace

struct spherekit_v8_engine {
  v8::Isolate* isolate = nullptr;
  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator;
  v8::Global<v8::Context> context;
  // Stable addresses handed to V8 as `External` data. Declared after the
  // isolate so the members are destroyed in reverse order: the isolate is gone,
  // and therefore no script can still be running, before any record dies.
  std::vector<std::unique_ptr<HostFunctionRecord>> host_functions;
};

namespace {

// Every entry point below needs the same four things before it can touch a
// context, so they are established once here.
struct EngineScope {
  v8::Isolate::Scope isolate_scope;
  v8::HandleScope handle_scope;
  v8::Local<v8::Context> context;
  v8::Context::Scope context_scope;
  v8::TryCatch try_catch;

  explicit EngineScope(spherekit_v8_engine* engine)
      : isolate_scope(engine->isolate),
        handle_scope(engine->isolate),
        context(engine->context.Get(engine->isolate)),
        context_scope(context),
        try_catch(engine->isolate) {}
};

int check_engine(spherekit_v8_engine* engine, spherekit_v8_error* error) {
  reset_error(error);
  if (engine == nullptr || engine->isolate == nullptr || engine->context.IsEmpty()) {
    return fail(error, kInvalidArgument, "V8 engine is null");
  }
  return kOk;
}

// Resolves `globalThis[name]`. Returns false with `status` set when the lookup
// itself failed; an absent property is a successful lookup yielding undefined.
bool lookup_global(spherekit_v8_engine* engine,
                   EngineScope& scope,
                   const uint8_t* name,
                   size_t name_length,
                   spherekit_v8_error* error,
                   v8::Local<v8::Value>* out,
                   int* status) {
  v8::Local<v8::String> key;
  if (!to_v8_string(engine->isolate, name, name_length, &key)) {
    *status = fail(error, kInvalidArgument, "V8 global name is not valid UTF-8");
    return false;
  }
  if (!scope.context->Global()->Get(scope.context, key).ToLocal(out)) {
    *status = report_exception(error, engine->isolate, scope.context, scope.try_catch);
    return false;
  }
  return true;
}

}  // namespace

extern "C" {

uint8_t* spherekit_v8_alloc(size_t size) {
  return static_cast<uint8_t*>(std::malloc(size == 0 ? 1 : size));
}

void spherekit_v8_free_buffer(uint8_t* buffer) {
  std::free(buffer);
}

const char* spherekit_v8_version() {
  return v8::V8::GetVersion();
}

int spherekit_v8_engine_new(const char* icu_data_path,
                            spherekit_v8_engine** output,
                            spherekit_v8_error* error) {
  reset_error(error);
  if (output == nullptr) {
    return fail(error, kInvalidArgument, "V8 engine output is null");
  }
  *output = nullptr;

  if (!initialize_v8(icu_data_path)) {
    return fail(error, kInitializationFailed, g_v8_initialization_error.c_str());
  }

  auto* engine = new (std::nothrow) spherekit_v8_engine();
  if (engine == nullptr) {
    return fail(error, kOutOfMemory, "V8 engine allocation failed");
  }

  engine->allocator.reset(v8::ArrayBuffer::Allocator::NewDefaultAllocator());
  if (!engine->allocator) {
    delete engine;
    return fail(error, kOutOfMemory, "V8 ArrayBuffer allocator allocation failed");
  }

  v8::Isolate::CreateParams params;
  params.array_buffer_allocator = engine->allocator.get();
  engine->isolate = v8::Isolate::New(params);
  if (engine->isolate == nullptr) {
    delete engine;
    return fail(error, kOutOfMemory, "V8 isolate creation failed");
  }

  // The host drives the frame loop, so it also decides when promise
  // continuations run. Under the default automatic policy a checkpoint fires
  // whenever script call depth returns to zero, which means a `.then` callback
  // can land in the middle of laying out a frame; explicit checkpoints move
  // that to a point the embedder chose.
  engine->isolate->SetMicrotasksPolicy(v8::MicrotasksPolicy::kExplicit);

  {
    v8::Isolate::Scope isolate_scope(engine->isolate);
    v8::HandleScope handle_scope(engine->isolate);
    v8::Local<v8::Context> context = v8::Context::New(engine->isolate);
    if (context.IsEmpty()) {
      engine->isolate->Dispose();
      delete engine;
      return fail(error, kInitializationFailed, "V8 context creation failed");
    }
    engine->context.Reset(engine->isolate, context);
  }

  *output = engine;
  return kOk;
}

void spherekit_v8_engine_free(spherekit_v8_engine* engine) {
  if (engine == nullptr) return;
  engine->context.Reset();
  if (engine->isolate != nullptr) engine->isolate->Dispose();
  delete engine;
}

int spherekit_v8_engine_eval(spherekit_v8_engine* engine,
                             const uint8_t* source,
                             size_t source_length,
                             const uint8_t* name,
                             size_t name_length,
                             uint8_t** result,
                             size_t* result_length,
                             spherekit_v8_error* error) {
  const int ready = check_engine(engine, error);
  if (ready != kOk) return ready;
  if (result == nullptr || result_length == nullptr) {
    return fail(error, kInvalidArgument, "V8 result output is null");
  }
  if (source == nullptr && source_length != 0) {
    return fail(error, kInvalidArgument, "V8 source is null");
  }
  if (source_length > static_cast<size_t>(std::numeric_limits<int>::max())) {
    return fail(error, kInvalidArgument, "V8 source is too large");
  }

  *result = nullptr;
  *result_length = 0;

  EngineScope scope(engine);
  v8::Local<v8::String> source_string;
  if (!to_v8_string(engine->isolate, source, source_length, &source_string)) {
    return fail(error, kInvalidArgument, "V8 source is not valid UTF-8");
  }
  v8::Local<v8::String> name_string;
  if (!to_v8_string(engine->isolate, name, name_length, &name_string)) {
    return fail(error, kInvalidArgument, "V8 script name is not valid UTF-8");
  }

  v8::ScriptOrigin origin(name_string);
  v8::Local<v8::Script> script;
  if (!v8::Script::Compile(scope.context, source_string, &origin).ToLocal(&script)) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }

  v8::Local<v8::Value> value;
  if (!script->Run(scope.context).ToLocal(&value)) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }

  v8::String::Utf8Value utf8(engine->isolate, value);
  if (*utf8 == nullptr) {
    return fail(error, kJavaScriptError, "V8 result conversion to UTF-8 failed");
  }
  if (!copy_bytes(*utf8, static_cast<size_t>(utf8.length()), result, result_length)) {
    return fail(error, kOutOfMemory, "V8 result allocation failed");
  }
  return kOk;
}

int spherekit_v8_engine_bind(spherekit_v8_engine* engine,
                             const uint8_t* name,
                             size_t name_length,
                             spherekit_v8_host_callback callback,
                             void* user_data,
                             spherekit_v8_error* error) {
  const int ready = check_engine(engine, error);
  if (ready != kOk) return ready;
  if (callback == nullptr) {
    return fail(error, kInvalidArgument, "V8 host callback is null");
  }

  EngineScope scope(engine);
  v8::Local<v8::String> key;
  if (!to_v8_string(engine->isolate, name, name_length, &key)) {
    return fail(error, kInvalidArgument, "V8 binding name is not valid UTF-8");
  }

  v8::Local<v8::Object> global = scope.context->Global();
  bool taken = false;
  if (!global->HasOwnProperty(scope.context, key).To(&taken)) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }
  if (taken) {
    return fail(error, kInvalidBinding, "name is already defined on globalThis");
  }

  auto record = std::make_unique<HostFunctionRecord>();
  record->callback = callback;
  record->user_data = user_data;

  v8::Local<v8::External> data =
      v8::External::New(engine->isolate, record.get(), kHostFunctionTag);
  v8::Local<v8::FunctionTemplate> tmpl =
      v8::FunctionTemplate::New(engine->isolate, host_function_callback, data);
  v8::Local<v8::Function> function;
  if (!tmpl->GetFunction(scope.context).ToLocal(&function)) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }
  function->SetName(key);

  bool defined = false;
  if (!global->Set(scope.context, key, function).To(&defined) || !defined) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }

  // Only now does the record become reachable from JavaScript, so only now is
  // it worth keeping alive. Every earlier return path destroys it instead.
  engine->host_functions.push_back(std::move(record));
  return kOk;
}

int spherekit_v8_engine_call_global(spherekit_v8_engine* engine,
                                    const uint8_t* name,
                                    size_t name_length,
                                    const uint8_t* argument,
                                    size_t argument_length,
                                    int32_t* found,
                                    uint8_t** result,
                                    size_t* result_length,
                                    spherekit_v8_error* error) {
  const int ready = check_engine(engine, error);
  if (ready != kOk) return ready;
  if (found == nullptr || result == nullptr || result_length == nullptr) {
    return fail(error, kInvalidArgument, "V8 call output is null");
  }
  *found = 0;
  *result = nullptr;
  *result_length = 0;

  EngineScope scope(engine);
  v8::Local<v8::Value> value;
  int status = kOk;
  if (!lookup_global(engine, scope, name, name_length, error, &value, &status)) return status;

  if (value->IsUndefined() || value->IsNull()) return kOk;
  if (!value->IsFunction()) {
    // Present but uncallable is a mistake worth surfacing: an optional callback
    // is absent, not a number.
    return fail(error, kInvalidArgument, "global is not a function");
  }

  v8::Local<v8::String> argument_string;
  if (!to_v8_string(engine->isolate, argument, argument_length, &argument_string)) {
    return fail(error, kInvalidArgument, "V8 argument is not valid UTF-8");
  }

  v8::Local<v8::Value> argv[1] = {argument_string};
  v8::Local<v8::Value> returned;
  if (!value.As<v8::Function>()->Call(scope.context, scope.context->Global(), 1, argv)
           .ToLocal(&returned)) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }

  v8::String::Utf8Value utf8(engine->isolate, returned);
  if (*utf8 == nullptr) {
    return fail(error, kJavaScriptError, "V8 result conversion to UTF-8 failed");
  }
  if (!copy_bytes(*utf8, static_cast<size_t>(utf8.length()), result, result_length)) {
    return fail(error, kOutOfMemory, "V8 result allocation failed");
  }
  *found = 1;
  return kOk;
}

int spherekit_v8_engine_has_global(spherekit_v8_engine* engine,
                                   const uint8_t* name,
                                   size_t name_length,
                                   int32_t* out,
                                   spherekit_v8_error* error) {
  const int ready = check_engine(engine, error);
  if (ready != kOk) return ready;
  if (out == nullptr) {
    return fail(error, kInvalidArgument, "V8 lookup output is null");
  }
  *out = 0;

  EngineScope scope(engine);
  v8::Local<v8::Value> value;
  int status = kOk;
  if (!lookup_global(engine, scope, name, name_length, error, &value, &status)) return status;
  *out = value->IsFunction() ? 1 : 0;
  return kOk;
}

int spherekit_v8_engine_run_microtasks(spherekit_v8_engine* engine, spherekit_v8_error* error) {
  const int ready = check_engine(engine, error);
  if (ready != kOk) return ready;

  EngineScope scope(engine);
  engine->isolate->PerformMicrotaskCheckpoint();
  if (scope.try_catch.HasCaught()) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }
  return kOk;
}

int spherekit_v8_engine_pump(spherekit_v8_engine* engine, spherekit_v8_error* error) {
  const int ready = check_engine(engine, error);
  if (ready != kOk) return ready;
  if (g_platform == nullptr) {
    return fail(error, kInitializationFailed, "V8 platform is not initialized");
  }

  EngineScope scope(engine);
  for (int task = 0; task < kMaxPumpedTasks; ++task) {
    if (!v8::platform::PumpMessageLoop(g_platform, engine->isolate)) break;
  }
  if (scope.try_catch.HasCaught()) {
    return report_exception(error, engine->isolate, scope.context, scope.try_catch);
  }
  return kOk;
}

}  // extern "C"
