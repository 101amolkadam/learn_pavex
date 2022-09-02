use clap::{Parser, Subcommand};
use pavex::App;
use pavex_builder::AppBlueprint;
use std::path::PathBuf;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    /// Expose inner details in case of an error.
    #[clap(long)]
    debug: bool,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate application runtime code according to an application blueprint.
    Generate {
        /// The source path for the serialized application blueprint.
        #[clap(short, long, value_parser)]
        blueprint: PathBuf,
        /// Optional. If provided, pavex will serialize diagnostic information about
        /// the application to the specified path.
        #[clap(long, value_parser)]
        diagnostics: Option<PathBuf>,
        /// The target directory for the generated application crate.  
        /// The path is interpreted as relative to the root of the current workspace.
        #[clap(short, long, value_parser)]
        output: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    miette::set_hook(Box::new(move |_| {
        let mut config = miette::MietteHandlerOpts::new();
        if cli.debug {
            config = config.with_cause_chain()
        } else {
            config = config.without_cause_chain()
        };
        Box::new(config.build())
    }))
    .unwrap();
    match cli.command {
        Commands::Generate {
            blueprint,
            diagnostics,
            output,
        } => {
            let blueprint = AppBlueprint::load(&blueprint)?;
            let app = App::build(blueprint)?;
            if let Some(diagnostic_path) = diagnostics {
                app.diagnostic_representation()
                    .persist_flat(&diagnostic_path)?;
            }
            assert!(
                output.is_relative(),
                "The output path must be relative to the root of the current `cargo` workspace."
            );
            let generated_app = app.codegen()?;
            generated_app.persist(&output)?;
        }
    }
    Ok(())
}