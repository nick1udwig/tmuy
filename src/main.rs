fn main() {
    if let Err(err) = tmuy::run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
