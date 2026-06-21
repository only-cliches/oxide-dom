fn main() {
    solite_build::workflow::bundle_for_cargo("ui", "drive_game_bundle.rs")
        .expect("bundle drive game UI");
}
