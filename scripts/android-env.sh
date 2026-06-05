#!/usr/bin/env bash
# Source before Gradle/adb from WSL:  source scripts/android-env.sh
# Picks JDK 17 + Android SDK from Linux or Windows (Android Studio) installs.

_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -n "${JAVA_HOME:-}" && ! -x "${JAVA_HOME}/bin/java" ]]; then
  unset JAVA_HOME
fi

if [[ -z "${JAVA_HOME:-}" ]]; then
  for candidate in \
    /usr/lib/jvm/java-17-openjdk-amd64 \
    /usr/lib/jvm/java-17-openjdk \
    "/mnt/c/Program Files/Android/Android Studio/jbr" \
    /mnt/c/Program\ Files/Android/Android\ Studio/jbr; do
    if [[ -d "$candidate" ]]; then
      if [[ -x "$candidate/bin/java" ]]; then
        export JAVA_HOME="$candidate"
        break
      fi
      if [[ -f "$candidate/bin/java.exe" ]]; then
        # WSL: Gradle expects bin/java; shim Windows Studio JBR.
        shim="$_root/scripts/.jdk-shim"
        mkdir -p "$shim/bin"
        cat >"$shim/bin/java" <<'EOF'
#!/usr/bin/env bash
jbr_exe="__JBR_EXE__"
args=()
convert_next=false
for arg in "$@"; do
  if [[ "$convert_next" == true ]]; then
    if [[ -e "$arg" ]]; then
      args+=("$(wslpath -w "$arg")")
    else
      args+=("$arg")
    fi
    convert_next=false
    continue
  fi
  case "$arg" in
    -classpath|-cp)
      args+=("$arg")
      convert_next=true
      ;;
    *)
      args+=("$arg")
      ;;
  esac
done
exec "$jbr_exe" "${args[@]}"
EOF
        sed -i "s|__JBR_EXE__|$candidate/bin/java.exe|" "$shim/bin/java"
        chmod +x "$shim/bin/java"
        export JAVA_HOME="$shim"
        break
      fi
    fi
  done
fi

if [[ -z "${ANDROID_HOME:-}" ]]; then
  for candidate in "$HOME/Android/Sdk"; do
    if [[ -d "$candidate" ]]; then
      export ANDROID_HOME="$candidate"
      break
    fi
  done
fi

if [[ -z "${ANDROID_HOME:-}" && -d /mnt/c/Users ]]; then
  for win_home in /mnt/c/Users/*; do
    [[ "$(basename "$win_home")" == "Public" ]] && continue
    candidate="$win_home/AppData/Local/Android/Sdk"
    if [[ -d "$candidate" ]]; then
      export ANDROID_HOME="$candidate"
      break
    fi
  done
fi

if [[ -n "${ANDROID_HOME:-}" ]]; then
  export PATH="$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$PATH"
  # WSL + Windows SDK ships adb.exe only — expose as `adb` for just/Gradle scripts.
  if [[ -f "$ANDROID_HOME/platform-tools/adb.exe" && ! -x "$ANDROID_HOME/platform-tools/adb" ]]; then
    export ADB="$ANDROID_HOME/platform-tools/adb.exe"
    adb() { "$ADB" "$@"; }
    export -f adb 2>/dev/null || true
  fi
  props="${ROOT_DIR:-$_root}/android/local.properties"
  if [[ ! -f "$props" ]]; then
    printf 'sdk.dir=%s\n' "$ANDROID_HOME" >"$props"
  fi
fi

if [[ -z "${JAVA_HOME:-}" ]]; then
  echo "android-env: JAVA_HOME not found — install JDK 17 or Android Studio" >&2
  exit 1
fi

if [[ -z "${ANDROID_HOME:-}" ]]; then
  echo "android-env: ANDROID_HOME not found — install Android Studio + SDK" >&2
  exit 1
fi
