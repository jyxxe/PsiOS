# Copy the artifact once so all verification steps read the same private bytes.
snapshot_attestation_file() {
    python3 -B "$(dirname "${BASH_SOURCE[0]}")/artifact_snapshot.py" "$1" "$2"
}

snapshot_attestation_arguments() {
    local -n snapshots=$1
    local work=$2
    shift 2
    if [[ $# -ge 1 ]]; then
        snapshot_attestation_file "$1" "$work/provenance.jsonl"
        snapshots+=("$work/provenance.jsonl")
    fi
    if [[ $# -eq 2 ]]; then
        snapshot_attestation_file "$2" "$work/trusted-root.jsonl"
        snapshots+=("$work/trusted-root.jsonl")
    fi
}

# Verify one file by its digest against a specific PsiOS main-branch workflow.
verify_core_attestation() {
    local workflow=$1 sha=$2 artifact=$3
    shift 3
    local repo=jyxxe/PsiOS
    local -a policy=(--repo "$repo" --signer-workflow "$repo/.github/workflows/$workflow"
        --source-ref refs/heads/main --source-digest "$sha" --deny-self-hosted-runners
        --predicate-type https://slsa.dev/provenance/v1)
    if [[ $# -ge 1 ]]; then policy+=(--bundle "$1"); fi
    if [[ $# -eq 2 ]]; then policy+=(--custom-trusted-root "$2"); fi
    gh attestation verify "$artifact" "${policy[@]}"
}
