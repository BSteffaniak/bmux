fn main() {
    let code = match bmux_bench_cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("bmux-bench: {error}");
            2
        }
    };
    std::process::exit(i32::from(code));
}
