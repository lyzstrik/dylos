#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo -e "\e[31m[ERREUR]\e[0m $1" >&2
    exit 1
}

# Chemins de référence
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOC_HOST="${REPO_ROOT}/docs/host.md"

# 1. Vérification /dev/kvm
echo "==> Vérification KVM..."
[ -e /dev/kvm ] || fail "/dev/kvm n'existe pas."
[ -r /dev/kvm ] && [ -w /dev/kvm ] || fail "L'utilisateur courant n'a pas les droits en lecture/écriture sur /dev/kvm."

# 2. Vérification Firecracker, Jailer et de leurs empreintes
echo "==> Vérification Firecracker et Jailer..."
FC_BIN="$(command -v firecracker 2>/dev/null)" || fail "Firecracker n'est pas installé dans le PATH."
JAILER_BIN="$(command -v jailer 2>/dev/null)" || fail "Jailer n'est pas installé dans le PATH."

echo "  - Firecracker : $(firecracker --version | head -n1)"
echo "  - Jailer      : $(jailer --version | head -n1)"

if [ -f "$DOC_HOST" ]; then
    echo "==> Vérification de la cohérence avec docs/host.md..."

    EXPECTED_FC_SHA="$(grep -E 'SHA-256 firecracker[[:space:]]*:' "$DOC_HOST" | head -n1 | sed -E 's/.*:[[:space:]]*([a-fA-F0-9]{64}).*/\1/')"
    EXPECTED_JAILER_SHA="$(grep -E 'SHA-256 jailer[[:space:]]*:' "$DOC_HOST" | head -n1 | sed -E 's/.*:[[:space:]]*([a-fA-F0-9]{64}).*/\1/')"

    if [ -n "$EXPECTED_FC_SHA" ]; then
        ACTUAL_FC_SHA="$(sha256sum "$FC_BIN" | awk '{print $1}')"
        if [ "$ACTUAL_FC_SHA" != "$EXPECTED_FC_SHA" ]; then
            fail "Désynchronisation Firecracker !\n  Attendu (docs/host.md) : $EXPECTED_FC_SHA\n  Calculé ($FC_BIN)   : $ACTUAL_FC_SHA"
        fi
        echo "  - SHA-256 Firecracker : OK"
    fi

    if [ -n "$EXPECTED_JAILER_SHA" ]; then
        ACTUAL_JAILER_SHA="$(sha256sum "$JAILER_BIN" | awk '{print $1}')"
        if [ "$ACTUAL_JAILER_SHA" != "$EXPECTED_JAILER_SHA" ]; then
            fail "Désynchronisation Jailer !\n  Attendu (docs/host.md) : $EXPECTED_JAILER_SHA\n  Calculé ($JAILER_BIN)   : $ACTUAL_JAILER_SHA"
        fi
        echo "  - SHA-256 Jailer      : OK"
    fi
else
    echo -e "\e[33m[AVERTISSEMENT]\e[0m $DOC_HOST introuvable, contrôle d'empreinte ignoré."
fi

# 3. Vérification du support reflink dans le repo (~/dylos/labs)
echo "==> Vérification support reflink..."
TARGET_DIR="${REPO_ROOT}/labs"
mkdir -p "$TARGET_DIR"

TMP_SRC=$(mktemp "$TARGET_DIR/reflink_test_src.XXXXXX")
TMP_DST=$(mktemp -u "$TARGET_DIR/reflink_test_dst.XXXXXX")

trap 'rm -f "$TMP_SRC" "$TMP_DST"' EXIT

echo "test-reflink" > "$TMP_SRC"

if ! cp --reflink=always "$TMP_SRC" "$TMP_DST" 2>/dev/null; then
    fail "Le système de fichiers sur $TARGET_DIR ne supporte pas --reflink=always."
fi

echo "  - Répertoire testé : $TARGET_DIR (reflink opérationnel)"
echo -e "\e[32m[SUCCÈS]\e[0m Tous les prérequis de l'hôte sont validés."
