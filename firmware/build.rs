fn main() {
    // Links the ESP-IDF build metadata into the binary; required by esp-idf-sys.
    embuild::espidf::sysenv::output();
}
