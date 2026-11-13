//! nova-edit: the standalone binary of the editor layout (also `novacode edit`).

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let target = std::env::args_os().nth(1).map(PathBuf::from);
    if let Err(e) = novacode_editor::run(target).await {
        eprintln!("nova-edit: {e}");
        std::process::exit(2);
    }
}
