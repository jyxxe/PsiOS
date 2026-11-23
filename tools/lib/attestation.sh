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
