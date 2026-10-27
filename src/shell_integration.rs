//! Startup files private to this child shell; user dotfiles remain untouched.
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1);
pub(crate) struct Integration {
    directory: PathBuf,
}
impl Integration {
    pub fn prepare(command: &mut Command) -> io::Result<Option<Self>> {
        let zsh = match Path::new(command.get_program())
            .file_name()
            .and_then(|name| name.to_str())
        {
            Some("zsh") => true,
            Some("bash") => false,
            _ => return Ok(None),
        };
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "HOME is not set for shell integration",
                )
            })?
            .join("nextty/shell");
        fs::create_dir_all(&root)?;
        let directory = root.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let integration = Self { directory };
        if zsh {
            let script = integration.directory.join("integration.zsh");
            fs::write(&script, ZSH)?;
            fs::write(integration.directory.join(".zshenv"), ZSHENV)?;
            command.env("NEXTTY_INTEGRATION", script);
            if let Some(original) = std::env::var_os("ZDOTDIR") {
                command
                    .env("NEXTTY_ZDOTDIR", original)
                    .env("NEXTTY_ZDOTDIR_SET", "1");
            } else {
                command
                    .env_remove("NEXTTY_ZDOTDIR")
                    .env_remove("NEXTTY_ZDOTDIR_SET");
            }
            command.env("ZDOTDIR", &integration.directory);
        } else {
            let script = integration.directory.join("bashrc");
            fs::write(&script, BASH)?;
            command.arg("--rcfile").arg(script).arg("-i");
        }
        Ok(Some(integration))
    }
}
impl Drop for Integration {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.directory)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!("clean shell integration: {e}");
        }
    }
}
const ZSHENV: &str = r#"if [[ $NEXTTY_ZDOTDIR_SET == 1 ]]; then
  ZDOTDIR=$NEXTTY_ZDOTDIR
else
  unset ZDOTDIR
fi
unset NEXTTY_ZDOTDIR NEXTTY_ZDOTDIR_SET
[[ -r ${ZDOTDIR-$HOME}/.zshenv ]] && source "${ZDOTDIR-$HOME}/.zshenv"
autoload -Uz add-zsh-hook
_nextty_initialize() {
  add-zsh-hook -d precmd _nextty_initialize
  source "$NEXTTY_INTEGRATION"
  unset NEXTTY_INTEGRATION
  _nextty_precmd
}
add-zsh-hook precmd _nextty_initialize
"#;
const ZSH: &str = r#"# ZLE otherwise keeps an unused cell after the right prompt.
ZLE_RPROMPT_INDENT=0
_nextty_cwd() {
  local LC_ALL=C value=$PWD char
  if [[ $value != *[^a-zA-Z0-9/._~-]* ]]; then
    printf '\033]7;file://localhost%s\007' "$value"
  else
    printf '\033]7;file://localhost'
    while [[ -n $value ]]; do
      char=${value[1]}; value=${value[2,-1]}
      case $char in [a-zA-Z0-9/._~-]) printf '%s' "$char" ;; *) printf '%%%02X' "'$char" ;; esac
    done
    printf '\007'
  fi
}
_nextty_precmd() {
  local nextty_exit_status=$?
  if [[ $_nextty_executing == 1 ]]; then printf '\033]133;D;%s\007' "$nextty_exit_status"; fi
  _nextty_executing=0
  _nextty_cwd
  local begin=$'%{\e]133;A\a%}' end=$'%{\e]133;B\a%}'
  [[ $PROMPT == $begin* ]] && PROMPT=${PROMPT#$begin}
  [[ $PROMPT == *$end ]] && PROMPT=${PROMPT%$end}
  PROMPT=$begin$PROMPT$end
  local scope_begin=$'%{\e]133;P;1\a%}' scope_end=$'%{\e]133;P;0\a%}'
  local right_begin=$'%{\e]133;P;2\a%}'
  [[ $RPROMPT == $scope_begin* ]] && RPROMPT=${RPROMPT#$scope_begin}
  [[ $RPROMPT == $right_begin* ]] && RPROMPT=${RPROMPT#$right_begin}
  [[ $RPROMPT == *$scope_end ]] && RPROMPT=${RPROMPT%$scope_end}
  RPROMPT=$right_begin$RPROMPT$scope_end
  [[ $PS2 == $scope_begin* ]] && PS2=${PS2#$scope_begin}
  [[ $PS2 == *$scope_end ]] && PS2=${PS2%$scope_end}
  PS2=$scope_begin$PS2$scope_end
  return "$nextty_exit_status"
}
_nextty_preexec() { printf '\033]133;C\007'; _nextty_executing=1 }
add-zsh-hook precmd _nextty_precmd
add-zsh-hook preexec _nextty_preexec
"#;
const BASH: &str = r#"[[ -r ~/.bashrc ]] && source ~/.bashrc
_nextty_precmd() {
  local nextty_exit_status=$?
  if [[ $_nextty_executing == 1 ]]; then printf '\033]133;D;%s\007' "$nextty_exit_status"; fi
  _nextty_executing=1
  local LC_ALL=C value=$PWD char
  if [[ $value != *[^a-zA-Z0-9/._~-]* ]]; then
    printf '\033]7;file://localhost%s\007' "$value"
  else
    printf '\033]7;file://localhost'
    while [[ -n $value ]]; do
      char=${value:0:1}; value=${value:1}
      case $char in [a-zA-Z0-9/._~-]) printf '%s' "$char" ;; *) printf '%%%02X' "'$char" ;; esac
    done
    printf '\007'
  fi
  local begin=$'\[\e]133;A\a\]' end=$'\[\e]133;B\a\]'
  [[ $PS1 == "$begin"* ]] && PS1=${PS1#"$begin"}
  [[ $PS1 == *"$end" ]] && PS1=${PS1%"$end"}
  PS1=$begin$PS1$end
  local scope_begin=$'\[\e]133;P;1\a\]' scope_end=$'\[\e]133;P;0\a\]'
  [[ $PS2 == "$scope_begin"* ]] && PS2=${PS2#"$scope_begin"}
  [[ $PS2 == *"$scope_end" ]] && PS2=${PS2%"$scope_end"}
  PS2=$scope_begin$PS2$scope_end
  return "$nextty_exit_status"
}
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == 'declare -a '* ]]; then
  PROMPT_COMMAND=(_nextty_precmd "${PROMPT_COMMAND[@]}")
else
  PROMPT_COMMAND=(_nextty_precmd "${PROMPT_COMMAND-}")
fi
PS0=$'\e]133;C\a'${PS0-}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Flags, WindowSize};
    use crate::terminal::{Shell, Terminal};
    use std::time::{Duration, Instant};

    #[test]
    fn zsh_right_prompt_reaches_last_pty_column_after_redraw_and_resize() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "target/zsh-right-prompt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let fixture = Integration { directory };
        let long_cwd = fixture.directory.join("long-directory-for-right-prompt");
        fs::create_dir(&long_cwd).unwrap();
        let script = fixture.directory.join("prompt.zsh");
        fs::write(
            &script,
            format!(
                "autoload -Uz add-zsh-hook\nsetopt PROMPT_SUBST\nPROMPT=' • '\nRPROMPT='%F{{#555555}}%~${{vcs_info_msg_0_}}%f'\nvcs_info_msg_0_=' 󱐋 main'\n{ZSH}"
            ),
        ).unwrap();
        let mut size = WindowSize {
            num_cols: 200,
            num_lines: 24,
            cell_width: 8,
            cell_height: 16,
        };
        let terminal = Terminal::new(
            Some(Shell::new("zsh".into(), vec!["-dfi".into()])),
            Some(PathBuf::from("/")),
            size,
            0xcccccc,
            0x202020,
        )
        .unwrap();
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        terminal
            .write(format!("source {}\n", quote(&script)).into_bytes())
            .unwrap();
        let prompt = |cwd: &Path, columns: usize, previous: Option<i32>| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                {
                    let core = terminal.core.lock().unwrap();
                    let cursor = core.cursor().0;
                    let row = core.row(cursor.line as usize);
                    let text: String = row.cells.iter().map(|cell| cell.c).collect();
                    if core.current_directory() == Some(cwd)
                        && core.columns() == columns
                        && previous != Some(cursor.line)
                        && text.ends_with("main")
                    {
                        assert_eq!(cursor.column, 3, "ZLE must restore the editing cursor");
                        assert!(
                            row.cells[columns - 4..].iter().all(|cell| cell
                                .flags
                                .contains(Flags::PROMPT | Flags::RIGHT_PROMPT))
                        );
                        return cursor.line;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "right prompt did not reach column {columns}; cwd={:?}, cursor={cursor:?}, row={text:?}",
                        core.current_directory()
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let row = prompt(Path::new("/"), 200, None);
        terminal.write(b"\n".as_slice()).unwrap();
        prompt(Path::new("/"), 200, Some(row));
        terminal
            .write(format!("cd {}\n", quote(&long_cwd)).into_bytes())
            .unwrap();
        prompt(&long_cwd, 200, None);
        size.num_cols = 160;
        terminal.resize(size).unwrap();
        prompt(&long_cwd, 160, None);
        terminal.write(b"cd /\n".as_slice()).unwrap();
        prompt(Path::new("/"), 160, None);
        size.num_cols = 40;
        terminal.resize(size).unwrap();
        let row = prompt(Path::new("/"), 40, None);
        terminal.write(b"\n".as_slice()).unwrap();
        prompt(Path::new("/"), 40, Some(row));
    }
}
