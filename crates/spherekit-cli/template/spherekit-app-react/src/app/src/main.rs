use {{RUST_CRATE_NAME}}_backend::AppBackend;

fn main() {
    let backend = AppBackend::new();
    println!("SphereKit React backend ready at revision {}", backend.react_tree().revision);
}
