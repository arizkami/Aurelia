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

namespace {

constexpr int kOk = 0;
constexpr int kInvalidArgument = 1;
constexpr int kInitializationFailed = 2;
constexpr int kOutOfMemory = 3;
constexpr int kJavaScriptError = 4;

std::once_flag g_v8_once;
// V8's platform must outlive all isolates and the V8 runtime. Intentionally
// leak it for the process lifetime so static teardown cannot destroy it after
// V8's own global state has already started shutting down.
v8::Platform* g_platform = nullptr;
std::string g_v8_initialization_error;

void write_error(char* output, size_t capacity, const std::string& message) {
  if (output == nullptr || capacity == 0) return;
  const size_t count = (message.size() < capacity - 1) ? message.size() : capacity - 1;
  std::memcpy(output, message.data(), count);
  output[count] = '\0';
}

int fail(char* error, size_t error_capacity, int status, const char* message) {
  write_error(error, error_capacity, message == nullptr ? "V8 operation failed" : message);
  return status;
}

std::string exception_message(v8::Isolate* isolate, v8::TryCatch& try_catch) {
  v8::String::Utf8Value exception(isolate, try_catch.Exception());
  if (*exception != nullptr && exception.length() != 0) {
    return std::string(*exception, exception.length());
  }
  return "JavaScript exception";
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

}  // namespace

struct spherekit_v8_engine {
  v8::Isolate* isolate = nullptr;
  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator;
  v8::Global<v8::Context> context;
};

extern "C" {

int spherekit_v8_engine_new(const char* icu_data_path,
                            spherekit_v8_engine** output,
                            char* error,
                            size_t error_capacity) {
  if (output == nullptr) {
    return fail(error, error_capacity, kInvalidArgument, "V8 engine output is null");
  }
  *output = nullptr;

  if (!initialize_v8(icu_data_path)) {
    return fail(error, error_capacity, kInitializationFailed,
                g_v8_initialization_error.c_str());
  }

  auto* engine = new (std::nothrow) spherekit_v8_engine();
  if (engine == nullptr) {
    return fail(error, error_capacity, kOutOfMemory, "V8 engine allocation failed");
  }

  engine->allocator.reset(v8::ArrayBuffer::Allocator::NewDefaultAllocator());
  if (!engine->allocator) {
    delete engine;
    return fail(error, error_capacity, kOutOfMemory,
                "V8 ArrayBuffer allocator allocation failed");
  }

  v8::Isolate::CreateParams params;
  params.array_buffer_allocator = engine->allocator.get();
  engine->isolate = v8::Isolate::New(params);
  if (engine->isolate == nullptr) {
    delete engine;
    return fail(error, error_capacity, kOutOfMemory, "V8 isolate creation failed");
  }

  {
    v8::Isolate::Scope isolate_scope(engine->isolate);
    v8::HandleScope handle_scope(engine->isolate);
    v8::Local<v8::Context> context = v8::Context::New(engine->isolate);
    if (context.IsEmpty()) {
      engine->isolate->Dispose();
      delete engine;
      return fail(error, error_capacity, kInitializationFailed,
                  "V8 context creation failed");
    }
    engine->context.Reset(engine->isolate, context);
  }

  *output = engine;
  return kOk;
}

void spherekit_v8_engine_free(spherekit_v8_engine* engine) {
  if (engine == nullptr) return;
  engine->context.Reset();
  engine->isolate->Dispose();
  delete engine;
}

int spherekit_v8_engine_eval(spherekit_v8_engine* engine,
                             const uint8_t* source,
                             size_t source_length,
                             uint8_t** result,
                             size_t* result_length,
                             char* error,
                             size_t error_capacity) {
  if (engine == nullptr || engine->isolate == nullptr) {
    return fail(error, error_capacity, kInvalidArgument, "V8 engine is null");
  }
  if (result == nullptr || result_length == nullptr) {
    return fail(error, error_capacity, kInvalidArgument, "V8 result output is null");
  }
  if (source == nullptr && source_length != 0) {
    return fail(error, error_capacity, kInvalidArgument, "V8 source is null");
  }
  if (source_length > static_cast<size_t>(std::numeric_limits<int>::max())) {
    return fail(error, error_capacity, kInvalidArgument, "V8 source is too large");
  }

  *result = nullptr;
  *result_length = 0;

  v8::Isolate::Scope isolate_scope(engine->isolate);
  v8::HandleScope handle_scope(engine->isolate);
  v8::Local<v8::Context> context = engine->context.Get(engine->isolate);
  if (context.IsEmpty()) {
    return fail(error, error_capacity, kInitializationFailed, "V8 context is empty");
  }
  v8::Context::Scope context_scope(context);
  v8::TryCatch try_catch(engine->isolate);

  const char* source_bytes = source == nullptr ? "" : reinterpret_cast<const char*>(source);
  v8::Local<v8::String> source_string;
  if (!v8::String::NewFromUtf8(engine->isolate, source_bytes,
                               v8::NewStringType::kNormal,
                               static_cast<int>(source_length))
           .ToLocal(&source_string)) {
    return fail(error, error_capacity, kJavaScriptError,
                "V8 source string creation failed");
  }

  v8::Local<v8::Script> script;
  if (!v8::Script::Compile(context, source_string).ToLocal(&script)) {
    const std::string message = exception_message(engine->isolate, try_catch);
    write_error(error, error_capacity, message);
    return kJavaScriptError;
  }

  v8::Local<v8::Value> value;
  if (!script->Run(context).ToLocal(&value)) {
    const std::string message = exception_message(engine->isolate, try_catch);
    write_error(error, error_capacity, message);
    return kJavaScriptError;
  }

  v8::String::Utf8Value utf8(engine->isolate, value);
  if (*utf8 == nullptr) {
    return fail(error, error_capacity, kJavaScriptError,
                "V8 result conversion to UTF-8 failed");
  }

  const size_t length = utf8.length();
  auto* bytes = static_cast<uint8_t*>(std::malloc(length == 0 ? 1 : length));
  if (bytes == nullptr) {
    return fail(error, error_capacity, kOutOfMemory, "V8 result allocation failed");
  }
  if (length != 0) std::memcpy(bytes, *utf8, length);
  *result = bytes;
  *result_length = length;
  return kOk;
}

void spherekit_v8_free_buffer(uint8_t* buffer) {
  std::free(buffer);
}

const char* spherekit_v8_version() {
  return v8::V8::GetVersion();
}

}  // extern "C"
