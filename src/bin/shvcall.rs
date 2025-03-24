use clap::Parser;
use log::*;
use shvrpc::util::parse_log_verbosity;
use simple_logger::SimpleLogger;

type Result = shvrpc::Result<()>;


fn main() -> Result {
    let opts = shvcall::Opts::parse();

    //println!("opts.version: {}", opts.version);
    let app_name = env!("CARGO_PKG_NAME");
    let app_version = env!("CARGO_PKG_VERSION");
    if opts.version {
        println!("{app_name} ver. {app_version}");
        return Ok(())
    }

    let mut logger = SimpleLogger::new();
    logger = logger.with_level(LevelFilter::Info);
    if let Some(module_names) = &opts.verbose {
        for (module, level) in parse_log_verbosity(module_names, module_path!()) {
            if let Some(module) = module {
                logger = logger.with_module_level(module, level);
            } else {
                logger = logger.with_level(level);
            }
        }
    }
    logger.init().unwrap();

    debug!("=====================================================");
    debug!("{app_name} ver. {app_version}");
    debug!("=====================================================");

    smol::block_on(shvcall::try_main(opts))
}
