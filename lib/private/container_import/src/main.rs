use camino::Utf8PathBuf;
use clap::{Subcommand, Parser};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Load{
        #[arg(long)]
        input: Utf8PathBuf,

        #[arg(long, env = "OUTPUT")]
        output: Utf8PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Load { input, output } => {
            eprintln!("Piping image from {} to {}", input, output);
            // pipe data from input (file descriptor) to output (file path)
            let mut input_file = std::fs::File::open(input).expect("Failed to open input file");
            let mut output_file = std::fs::File::create(output).expect("Failed to create output file");
            std::io::copy(&mut input_file, &mut output_file).expect("Failed to copy data");
            eprintln!("Done.");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
