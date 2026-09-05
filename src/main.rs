use std::env;
use std::path::PathBuf;

use xdir::core::navigation::Navigation;

fn main() {
    let start_dir = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().expect("cannot determine current directory"));

    let navigation = match Navigation::new(start_dir) {
        Ok(navigation) => navigation,
        Err(err) => {
            eprintln!("xdir: {err}");
            std::process::exit(1);
        }
    };

    println!("xdir core (Milestone 0) — sem interface gráfica");
    println!("current: {}", navigation.current_dir().display());

    match navigation.entries() {
        Ok(entries) => {
            for entry in entries {
                println!("  {}", entry.name().to_string_lossy());
            }
        }
        Err(err) => eprintln!("xdir: erro ao ler diretório: {err}"),
    }
}
