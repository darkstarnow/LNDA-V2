#[path = "Census.rs"]
mod census;
#[path = "Inspect.rs"]
mod inspect;
#[path = "Run.rs"]
mod run;
#[path = "Shader.rs"]
mod shader;
#[path = "Translate.rs"]
mod translate;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "leonida",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Show the headers, sections, imports, TLS and build identity of a PE image")]
    Inspect {
        file: PathBuf,
        #[arg(long, help = "List every imported function")]
        imports: bool,
        #[arg(long, help = "List every exported function")]
        exports: bool,
    },
    #[command(about = "Count the imports of every PE image and how many Leonida covers")]
    Census {
        paths: Vec<PathBuf>,
        #[arg(long, help = "Treat host pass-through imports as missing")]
        strict: bool,
        #[arg(long, help = "List every missing import, most used first")]
        missing: bool,
    },
    #[command(about = "List the exports an import library describes")]
    LibExports {
        lib: PathBuf,
        #[arg(long, help = "List every export instead of per-DLL counts")]
        list: bool,
    },
    #[command(about = "Find shader containers and report their layout")]
    ShaderInfo {
        paths: Vec<PathBuf>,
        #[arg(long, help = "Print one line per container")]
        verbose: bool,
        #[arg(long, help = "Also print the decoded native instructions")]
        disasm: bool,
    },
    #[command(about = "Translate every shader found and report what is not supported yet")]
    Translate {
        paths: Vec<PathBuf>,
        #[arg(long, help = "Check the emitted DXIL with the DXIL validator")]
        validate: bool,
        #[arg(long, help = "Also create a host D3D12 pipeline for each shader")]
        pipelines: bool,
        #[arg(long, help = "Print one line per program and outcome")]
        list: bool,
        #[arg(long, help = "Worker threads, every logical core by default")]
        jobs: Option<usize>,
    },
    #[command(about = "Translate a title's shaders into the cache ahead of play")]
    Warm {
        paths: Vec<PathBuf>,
        #[arg(long, help = "Worker threads, every logical core by default")]
        jobs: Option<usize>,
        #[arg(long, help = "Skip creating compute pipelines")]
        no_pipelines: bool,
    },
    #[command(about = "Prepare, bind and optionally start a title executable")]
    Run {
        exe: PathBuf,
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap, help = "What an unimplemented import does when called")]
        unimplemented: Unimplemented,
        #[arg(long, help = "Treat host pass-through imports as missing")]
        strict: bool,
        #[arg(long, help = "Start the title after binding")]
        execute: bool,
        #[arg(long, value_enum, default_value_t = Console::SeriesX, help = "Console model to report")]
        console: Console,
        #[arg(last = true, help = "Arguments passed to the title")]
        title_args: Vec<String>,
    },
    #[command(about = "Show the configured game, cache and data directories")]
    Config,
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn main() -> Result<()> {
    leonida_core::logging::init("info");
    #[cfg(all(windows, feature = "diagnostics"))]
    if leonida_services::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
        } => {
            if let Some(n) = std::env::var("LEONIDA_VIEW_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                leonida_shader::emit::set_view_heap_limit(n);
            }
            if let Some(n) = std::env::var("LEONIDA_HOST_WAVE")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                leonida_shader::emit::set_host_max_wave(n);
            }
            translate::run(paths, validate, pipelines, list, jobs)
        }
        Command::Warm {
            paths,
            jobs,
            no_pipelines,
        } => translate::warm(paths, jobs, !no_pipelines),
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args: _,
        } => {
            let result = run::run(&exe, unimplemented, strict, execute, console);
            let code = if result.is_ok() { 0 } else { 1 };
            tracing::info!("runtime exit: code {code}");
            result
        }
        Command::Config => show_config(),
    }
}

fn show_config() -> Result<()> {
    let cfg = leonida_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = leonida_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = leonida_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see LeonidaExample.toml)"
    );
    Ok(all)
}
