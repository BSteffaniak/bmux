if [[ -o interactive && -n ${APPLICATION_RESURRECTION_HOST:-} && -n ${BMUX_SHELL_CALLBACK:-} && ${__BMUX_NESTED_PID:-} != $$ ]]; then
  __BMUX_NESTED_PID=$$
  __BMUX_NESTED_EXECUTABLE=${commands[zsh]}
  __bmux_nested_precmd() {
    local cwd
    cwd=$("$BMUX_SHELL_CALLBACK" __shell-invocation location "$__BMUX_NESTED_EXECUTABLE" "$$" '' "$PWD") || return
    if [[ -n $cwd ]]; then builtin cd -- "$cwd" || return; fi
    "$BMUX_SHELL_CALLBACK" __shell-invocation prompt "$__BMUX_NESTED_EXECUTABLE" "$$" '' "$PWD"
  }
  __bmux_nested_preexec() {
    "$BMUX_SHELL_CALLBACK" __shell-invocation start "$__BMUX_NESTED_EXECUTABLE" "$$" "$1" "$PWD"
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __bmux_nested_precmd
  add-zsh-hook preexec __bmux_nested_preexec
fi
