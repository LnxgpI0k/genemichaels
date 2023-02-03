use anyhow::{
    anyhow,
    Error,
};
use clap::Parser;
use genemichaels::{
    format_str,
    print_error_text,
    print_skipping_text,
    Comment,
    FormatConfig,
};
use threadpool::ThreadPool;
use std::{
    collections::HashSet,
    env::current_dir,
    fmt::Display,
    fs,
    io::Read,
    path::{
        Path,
        PathBuf,
    },
    process,
    result,
    str::FromStr,
    time,
    ffi::OsStr,
    sync::{
        Arc,
        Mutex,
    },
};
use syn::File;

type Result<T> = result::Result<T, anyhow::Error>;
const CARGO_TOML: &str = "Cargo.toml";

#[derive(Clone)]
enum Offable<T> {
    Off,
    On(T),
}

impl<T: Display> std::fmt::Display for Offable<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Offable::Off => "off".fmt(f),
            Offable::On(x) => x.fmt(f),
        }
    }
}

impl<E: std::error::Error + Send + Sync + 'static, T: FromStr<Err = E> + Clone + Display> FromStr for Offable<T> {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> result::Result<Self, Self::Err> {
        if s == "off" {
            Ok(Self::Off)
        } else {
            Ok(Self::On(T::from_str(s)?))
        }
    }
}
#[derive(Clone)]
struct On;

impl std::fmt::Display for On {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "on".fmt(f)
    }
}

#[derive(Debug)]
struct OnErr(String);

impl Display for OnErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for OnErr {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl FromStr for On {
    type Err = OnErr;

    fn from_str(s: &str) -> result::Result<Self, Self::Err> {
        if s == "on" {
            Ok(On)
        } else {
            Err(OnErr(format!("[{}] not allowed, must be on or off", s)))
        }
    }
}

#[derive(Parser, Clone)]
struct Args {
    #[arg(help = "Files to format in place; if none specified formats stdin and writes result to stdout")]
    files: Vec<PathBuf>,
    #[arg(short, long, help = "Won't emit any output")]
    quiet: bool,
    #[arg(short, long, help = "Formats the entire package using the Cargo.toml")]
    package: bool,
    #[arg(long, help = "Limits threads to specified count when using --package")]
    thread_count: Option<usize>,
    #[arg(short, long, default_value_t = FormatConfig::default().max_width)]
    line_length: usize,
    #[arg(long, help = "For any node that's split, all parent nodes must also be split")]
    root_splits: bool,
    #[arg(
        long,
        help = "Always split {} groups with >= this number of children; disable with `off`",
        default_value_t = match FormatConfig::default().split_brace_threshold {
            Some(x) => Offable::On(x),
            None => Offable::Off,
        },
    )]
    split_brace_threshold: Offable<usize>,
    #[arg(
        long,
        help = "Always split #[] attributes; disable with `off`",
        default_value_t = match FormatConfig::default().split_attributes {
            true => Offable::On(On),
            false => Offable::Off,
        },
    )]
    split_attributes: Offable<On>,
    #[arg(
        long,
        help = "Always split where clauses; disable with `off`",
        default_value_t = match FormatConfig::default().split_attributes {
            true => Offable::On(On),
            false => Offable::Off,
        },
    )]
    split_where: Offable<On>,
    #[arg(
        long,
        help = "Use a max comment length relative to start of comment (i.e. ignoring indentation); disable with `off`",
        default_value_t = match FormatConfig::default().comment_width {
            Some(x) => Offable::On(x),
            None => Offable::Off,
        },
    )]
    comment_length: Offable<usize>,
    #[arg(
        long,
        help = "Problems formatting comments are fatal; disable with `false`",
        default_value_t = match FormatConfig::default().comment_errors_fatal {
            true => Offable::On(On),
            false => Offable::Off,
        },
    )]
    comment_errors_fatal: Offable<On>,
}

fn skip(src: &str) -> bool {
    src.lines().take(5).any(|l| l.contains("`nogenemichaels`"))
}

fn process(config: &FormatConfig, source: &str) -> Result<String> {
    let res = format_str(source, config)?;
    if !res.lost_comments.is_empty() {
        return Err(
            anyhow!(
                "The following comments were missed during formatting: {:?}",
                res.lost_comments.values().flatten().collect::<Vec<&Comment>>()
            ),
        );
    }
    match syn::parse_str::<File>(&res.rendered) {
        Ok(_) => { },
        Err(e) => {
            return Err(
                anyhow!(
                    "Rendered document couldn't be re-parsed in verification step at {}:{}: {}\n\n{}",
                    e.span().start().line,
                    e.span().start().column,
                    e,
                    res
                        .rendered
                        .lines()
                        .enumerate()
                        .skip(e.span().start().line.saturating_sub(5))
                        .take(10)
                        .map(|(ln, l)| format!("{:0>4} {}", ln + 1, l))
                        .collect::<Vec<String>>()
                        .join("\n")
                ),
            );
        },
    };
    Ok(res.rendered)
}

fn main() {
    let args = Args::parse();
    let config = FormatConfig {
        quiet: args.quiet,
        max_width: args.line_length,
        root_splits: args.root_splits,
        split_brace_threshold: match args.split_brace_threshold {
            Offable::Off => None,
            Offable::On(n) => Some(n),
        },
        split_attributes: match args.split_attributes {
            Offable::Off => false,
            Offable::On(_) => true,
        },
        split_where: match args.split_where {
            Offable::Off => false,
            Offable::On(_) => true,
        },
        comment_width: match args.comment_length {
            Offable::Off => None,
            Offable::On(n) => Some(n),
        },
        comment_errors_fatal: match args.comment_errors_fatal {
            Offable::Off => false,
            Offable::On(_) => true,
        },
    };
    if args.package {
        let res = || -> Result<()> {
            {
                let inst = time::Instant::now();
                eprintln!("\x1B[1;32m  Formatting\x1B[0;22m workspace...");
                let mut project_cargo_toml = None;
                let c_dir = current_dir()?;
                let mut at: Option<&Path> = Some(&c_dir);
                while let Some(d) = at.take() {
                    let cargo_toml_path = d.join(CARGO_TOML);
                    if cargo_toml_path.exists() {
                        project_cargo_toml = Some(cargo_toml_path);
                        break;
                    }
                    at = d.parent();
                }
                let mut pool = FormatPool {
                    config,
                    seen: HashSet::new(),
                    pool: {
                        let mut p = threadpool::Builder::new();
                        if let Some(t) = args.thread_count {
                            p = p.num_threads(t);
                        }
                        p.build()
                    },
                    errors: Arc::new(Mutex::new(vec![])),
                };
                process_dirs(
                    &mut pool,
                    &project_cargo_toml.ok_or_else(|| anyhow::anyhow!("No Cargo.toml found!"))?,
                )?;
                pool.join()?;
                eprintln!(
                    "\x1B[1;32m    Finished\x1B[0;22m workspace formatting successfully in {:.2}s",
                    time::Instant::now().duration_since(inst).as_secs_f64()
                );
                Result::Ok(())
            }
        };
        match res() {
            Ok(_) => { },
            Err(e) => {
                print_error_text();
                eprintln!("formatting: {:?}", e);
                process::exit(1);
            },
        };
    } else if args.files.is_empty() {
        let inst = time::Instant::now();
        let res = || -> Result<()> {
            let mut source = Vec::new();
            std::io::stdin().read_to_end(&mut source)?;
            let source = String::from_utf8(source)?;
            if skip(&source) {
                print!("{}", source);
                anyhow::Ok(())
            } else {
                let out = process(&config, &source)?;
                print!("{}", out);
                anyhow::Ok(())
            }
        };
        match res() {
            Ok(_) => { },
            Err(e) => {
                if !args.quiet {
                    print_error_text();
                    eprintln!("formatting stdin: {:?}", e);
                }
                process::exit(1);
            },
        };
        eprintln!(
            "\x1B[1;32m    Finished\x1B[0;22m workspace formatted successfully in {:.2}s",
            time::Instant::now().duration_since(inst).as_secs_f64()
        );
    } else {
        let inst = time::Instant::now();
        let mut failed = false;
        for file in &args.files {
            let res = || -> Result<()> {
                let source = String::from_utf8(fs::read(file)?)?;
                if skip(&source) {
                    print_skipping_text();
                    eprintln!("{}", &file.to_string_lossy());
                    return Ok(());
                }
                if !args.quiet {
                    eprintln!("\x1B[1;32m  Formatting\x1B[0;22m {}", &file.to_string_lossy());
                };
                let out = process(&config, &source)?;
                fs::write(file, out.as_bytes())?;
                Ok(())
            };
            match res() {
                Ok(_) => { },
                Err(e) => {
                    if !args.quiet {
                        print_error_text();
                        eprintln!("formatting {}: {:?}", &file.to_string_lossy(), e);
                    }
                    failed = true;
                },
            };
        }
        eprintln!(
            "\x1B[1;32m    Finished\x1B[0;22m workspace formatted successfully in {:.2}s",
            time::Instant::now().duration_since(inst).as_secs_f64()
        );
        if failed {
            process::exit(1);
        }
    }
}

fn process_dirs(pool: &mut FormatPool, manifest_path: &Path) -> Result<()> {
    let manifest = cargo_manifest::Manifest::from_path(manifest_path)?;
    let path = manifest_path.parent().unwrap();
    for bin in manifest.bin.into_iter().flatten() {
        if let Some(bin_path) = bin.path {
            pool.format_dir(path.join(bin_path).parent().unwrap().to_owned());
        }
    }
    if let Some(lib) = manifest.lib {
        if let Some(lib_path) = lib.path {
            pool.format_dir(path.join(lib_path).parent().unwrap().to_owned());
        }
    }
    for bench in manifest.bench.into_iter().flatten() {
        if let Some(bench_path) = bench.path {
            pool.format_dir(path.join(bench_path).parent().unwrap().to_owned());
        }
    }
    for test in manifest.test.into_iter().flatten() {
        if let Some(test_path) = test.path {
            pool.format_dir(path.join(test_path).parent().unwrap().to_owned());
        }
    }
    for example in manifest.example.into_iter().flatten() {
        if let Some(example_path) = example.path {
            pool.format_dir(path.join(example_path).parent().unwrap().to_owned());
        }
    }
    if let Some(ws) = manifest.workspace {
        let workspace_dirs: Vec<PathBuf> =
            ws.members.into_iter().filter_map(|m| path.join(&m).exists().then(|| path.join(m))).collect();

        // loop through each folder in the workspace and recursively run the formatter
        for workspace in workspace_dirs {
            process_dirs(pool, &path.join(workspace).join(CARGO_TOML))?;
        }
    };

    // default bins location
    if path.join("bin").exists() {
        pool.format_dir(path.join("bin"));
    }

    // default benches location
    if path.join("benches").exists() {
        pool.format_dir(path.join("benches"));
    }

    // default tests location
    if path.join("tests").exists() {
        pool.format_dir(path.join("tests"));
    }

    // default examples location
    if path.join("examples").exists() {
        pool.format_dir(path.join("examples"));
    }

    // add src if exists
    if path.join("src").exists() {
        pool.format_dir(path.join("src"));
    };
    Ok(())
}

struct FormatPool {
    config: FormatConfig,
    seen: HashSet<PathBuf>,
    pool: ThreadPool,
    errors: Arc<Mutex<Vec<Error>>>,
}

impl FormatPool {
    fn format_dir(&mut self, dir: PathBuf) {
        if !self.seen.insert(dir.clone()) {
            return;
        }
        for f in walkdir::WalkDir::new(&dir) {
            match f {
                Ok(file) => {
                    let file_path = file.path().to_path_buf();
                    if !self.seen.insert(file_path.clone()) || file_path.extension() != Some(OsStr::new("rs")) {
                        continue;
                    }
                    let config = self.config.clone();
                    let errors = self.errors.clone();
                    self.pool.execute(move || {
                        let res = || -> Result<()> {
                            let source = fs::read_to_string(file_path.clone())?;
                            if skip(&source) {
                                print_skipping_text();
                                eprintln!("{}", &file_path.to_string_lossy());
                                return Result::Ok(());
                            }
                            fs::write(&file_path, process(&config, &source)?.as_bytes())?;
                            Ok(())
                        };
                        match res() {
                            Ok(_) => {
                                eprintln!("\x1B[1;32m   Formatted\x1B[0;22m {}", file_path.to_string_lossy());
                            },
                            Err(e) => {
                                errors
                                    .lock()
                                    .unwrap()
                                    .push(e.context(format!("Error formatting {}", file_path.to_string_lossy())));
                            },
                        }
                    });
                },
                Err(e) => {
                    eprintln!("Error while scanning dir {}: {}", dir.to_string_lossy(), e);
                    continue;
                },
            }
        }
    }

    fn join(&mut self) -> Result<()> {
        self.pool.join();
        if self.pool.panic_count() > 0 {
            return Err(anyhow!("Panic during formatting."));
        }
        let errors = self.errors.lock().unwrap();
        if !errors.is_empty() {
            for e in errors.iter() {
                print_error_text();
                eprintln!("{}", e.to_string());
            }
            return Err(anyhow!("Errors encountered during formatting."));
        }
        Ok(())
    }
}
