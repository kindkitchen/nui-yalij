use common.nu [checked root]

# Validate both the application and exactly the source package users receive.
def main [] {
    cd $root
    checked cargo fmt --all -- --check
    checked cargo clippy --locked --all-targets -- -D warnings
    # PTY forks can temporarily inherit another concurrent test's file lock.
    checked cargo test --locked --all-targets -- --test-threads=1
    checked nu release/test.nu
    let files = checked cargo package --list --locked --allow-dirty | lines
    if ($files | any {|file| ($file | path basename | str starts-with '.env') }) {
        error make --unspanned {msg: "refusing to package a dotenv file"}
    }
    # Version preparation changes only the two version files before this check.
    checked cargo publish --dry-run --locked --allow-dirty
}
