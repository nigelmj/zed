use std::{fs::File, path::Path};

use db::pane::{DockAnchor, SerializedDockPane};

const TEST_FILE: &'static str = "test-db.db";

fn main() -> anyhow::Result<()> {
    let db = db::Db::open_in_memory();
    if db.real().is_none() {
        return Err(anyhow::anyhow!("Migrations failed"));
    }
    let file = Path::new(TEST_FILE);

    let f = File::create(file)?;
    drop(f);

    let workspace = db.workspace_for_roots(&["/tmp"]);

    db.save_dock_pane(SerializedDockPane {
        workspace: workspace.workspace_id,
        anchor_position: DockAnchor::Expanded,
        shown: true,
    });

    let _new_workspace = db.workspace_for_roots(&["/tmp"]);

    db.write_to(file).ok();

    println!("Wrote database!");

    Ok(())
}
