#!/usr/bin/env bash
set -eu

zshrc_path="${HOME}/.zshrc"

cat >"${zshrc_path}" <<EOF
export PATH="\$HOME/.local/bin:\$HOME/.cargo/bin:\$PATH"
export EDITOR="nvim"
export VISUAL="nvim"
alias vi="nvim"
alias vim="nvim"

if command -v mise >/dev/null 2>&1; then
  eval "\$(mise activate zsh)"
fi

if command -v direnv >/dev/null 2>&1; then
  eval "\$(direnv hook zsh)"
fi

ZINIT_HOME="\${XDG_DATA_HOME:-\${HOME}/.local/share}/zinit/zinit.git"
if [ ! -f "\${ZINIT_HOME}/zinit.zsh" ]; then
  mkdir -p "\${ZINIT_HOME%/*}"
  git clone https://github.com/zdharma-continuum/zinit.git "\${ZINIT_HOME}"
fi

if [ -f "\${ZINIT_HOME}/zinit.zsh" ]; then
  source "\${ZINIT_HOME}/zinit.zsh"
  autoload -Uz compinit && compinit

  zinit for \\
    atload"zicompinit; zicdreplay" \\
    blockf \\
    lucid \\
    wait \\
    zsh-users/zsh-completions

  zinit light zsh-users/zsh-syntax-highlighting
  zinit light nbari/slick
fi

(( \${+ZSH_HIGHLIGHT_STYLES} )) || typeset -A ZSH_HIGHLIGHT_STYLES
ZSH_HIGHLIGHT_STYLES[path]=none
ZSH_HIGHLIGHT_STYLES[path_prefix]=none

if command -v slick >/dev/null 2>&1; then
  export SLICK_PATH="\$(command -v slick)"
fi
EOF

chmod 0644 "${zshrc_path}"
