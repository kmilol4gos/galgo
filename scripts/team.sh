#!/usr/bin/env bash
# Levanta un equipo de agentes en paneles de galgo:
#   <prefijo>-manager     Claude Code (modelo por defecto)
#   <prefijo>-planner     PI con el modelo que indiques (p. ej. GPT Terra)
#   <prefijo>-programmer  Claude Code con Haiku
#   <prefijo>-reviewer    Claude Code con Opus
# Los tres últimos reportan al manager.
#
# Uso:    scripts/team.sh <prefijo> [directorio]
# Ajustes (variables de entorno):
#   PLANNER_MODEL     modelo de PI, como lo muestra /model en PI (p. ej. openai/<id de GPT Terra>).
#                     Vacío: el modelo que PI tenga guardado por defecto.
#   MANAGER_MODEL     modelo del manager (vacío: el de Claude Code).
#   PROGRAMMER_MODEL  por defecto: haiku
#   REVIEWER_MODEL    por defecto: opus
#   GALGO_INJECT=1     el daemon escribe en el manager los resultados pendientes cuando está idle.
#                     Requiere los hooks instalados (galgo hooks install claude); sin ellos el
#                     estado nunca cambia y podría escribir en medio de un turno.
#   CLAUDE_BIN, PI_BIN  rutas de los ejecutables (por defecto: claude, pi).
set -eu

PREFIX=${1:?uso: scripts/team.sh <prefijo> [directorio]}
DIR=$(cd "${2:-$PWD}" && pwd -P)
CLAUDE=${CLAUDE_BIN:-claude}
PI=${PI_BIN:-pi}
PROGRAMMER_MODEL=${PROGRAMMER_MODEL:-haiku}
REVIEWER_MODEL=${REVIEWER_MODEL:-opus}
INJECT=
[ "${GALGO_INJECT:-}" != 1 ] || INJECT=--inject

spawn() { # nombre, luego las opciones de `galgo spawn`
  local name=$1
  shift
  galgo spawn "$PREFIX-$name" --cwd "$DIR" "$@"
}

spawn manager $INJECT -- "$CLAUDE" ${MANAGER_MODEL:+--model "$MANAGER_MODEL"}
spawn planner --manager "$PREFIX-manager" -- "$PI" ${PLANNER_MODEL:+--model "$PLANNER_MODEL"}
spawn programmer --manager "$PREFIX-manager" -- "$CLAUDE" --model "$PROGRAMMER_MODEL"
spawn reviewer --manager "$PREFIX-manager" -- "$CLAUDE" --model "$REVIEWER_MODEL"

echo
echo "Equipo '$PREFIX' listo. Estado: galgo ls"
echo "Nota: los hooks de galgo cubren Claude Code. PI debe reportar con 'galgo done --summary ...'."
