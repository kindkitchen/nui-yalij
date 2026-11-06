use common.nu [checked verify-tag root]

# Called only after all matching GitHub Release assets have been attached.
def main [tag: string] {
    cd $root
    verify-tag $tag | ignore
    if ($env.CARGO_REGISTRY_TOKEN? | default '' | is-empty) {
        error make --unspanned {msg: 'configure the CARGO_REGISTRY_TOKEN repository secret'}
    }
    checked cargo publish --locked
}
