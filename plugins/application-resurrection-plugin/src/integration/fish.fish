if status is-interactive; and set -q APPLICATION_RESURRECTION_HOST; and set -q BMUX_SHELL_CALLBACK; and not set -q __bmux_nested_installed
  set -g __bmux_nested_installed 1
  set -g __bmux_nested_executable (status fish-path)
  function __bmux_nested_prompt --on-event fish_prompt
    set -l cwd ("$BMUX_SHELL_CALLBACK" __shell-invocation location "$__bmux_nested_executable" "$fish_pid" '' "$PWD")
    if test $status -ne 0; return; end
    if test -n "$cwd"; builtin cd -- "$cwd"; or return; end
    "$BMUX_SHELL_CALLBACK" __shell-invocation prompt "$__bmux_nested_executable" "$fish_pid" '' "$PWD"
  end
  function __bmux_nested_preexec --on-event fish_preexec
    "$BMUX_SHELL_CALLBACK" __shell-invocation start "$__bmux_nested_executable" "$fish_pid" "$argv[1]" "$PWD"
  end
end
