fn main() {
    viy_cli::hardening::pre_main();
    clap_complete::CompleteEnv::with_factory(viy_cli::cli::command)
        .var(viy_cli::cli::COMPLETION_ENV)
        .complete();
    let code = viy_cli::run(
        std::env::args_os().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        viy_cli::Terminal::detect(),
    );
    std::process::exit(code);
}
