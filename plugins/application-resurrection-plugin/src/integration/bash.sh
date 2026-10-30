# Loaded by an interactive nested Bash, never by noninteractive scripts.
if [[ $- == *i* && -n ${APPLICATION_RESURRECTION_HOST:-} && -n ${BMUX_SHELL_CALLBACK:-} && ${__BMUX_NESTED_PID:-} != $$ ]]; then
  __BMUX_NESTED_PID=$$
  __BMUX_NESTED_EXECUTABLE="${BASH}"
  __bmux_nested_precmd() {
    local cwd
    cwd=$("$BMUX_SHELL_CALLBACK" __shell-invocation location "$__BMUX_NESTED_EXECUTABLE" "$$" '' "$PWD") || return
    if [[ -n $cwd ]]; then builtin cd -- "$cwd" || return; fi
    "$BMUX_SHELL_CALLBACK" __shell-invocation prompt "$__BMUX_NESTED_EXECUTABLE" "$$" '' "$PWD"
    __BMUX_NESTED_READY=1
  }
  __bmux_nested_preexec() {
    [[ ${__BMUX_NESTED_READY:-0} == 1 ]] || return
    __BMUX_NESTED_READY=0
    local cmd=${1:-}
    if [[ -z $cmd ]]; then
      cmd=$(HISTTIMEFORMAT= builtin history 1)
      if [[ $cmd =~ ^[[:space:]]*[0-9]+[[:space:]]+(.*)$ ]]; then cmd=${BASH_REMATCH[1]}; fi
    fi
    [[ -n $cmd ]] && "$BMUX_SHELL_CALLBACK" __shell-invocation start "$__BMUX_NESTED_EXECUTABLE" "$$" "$cmd" "$PWD"
  }
  __bmux_nested_bp_preexec() { __BMUX_NESTED_READY=1; __bmux_nested_preexec "$1"; }
  preexec_functions+=(__bmux_nested_bp_preexec)
  PROMPT_COMMAND+=(__bmux_nested_precmd)
  trap '__bmux_nested_preexec' DEBUG
fi
