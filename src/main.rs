use std::env;
use std::path::PathBuf;

use xdir::app::AppState;
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

    let state = AppState::new(navigation);

    if let Err(err) = xdir::ui::run(state) {
        eprintln!("xdir: {err}");
        std::process::exit(1);
    }
}
