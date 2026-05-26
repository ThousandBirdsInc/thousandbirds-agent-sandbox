fn main() {
    let code = match thousandbirds_agent_sandbox::cli::run(std::env::args().collect()) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("tb-sandbox: {err}");
            1
        }
    };
    std::process::exit(code);
}
