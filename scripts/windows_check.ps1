param(
    [ValidateSet("lint", "check")]
    [string]$Mode = "check"
)

$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code $LASTEXITCODE`: $Command $($Arguments -join ' ')"
    }
}

# Fork change (bd herdr-zs55): the retry is allowed ONLY for a transient Zig package fetch,
# with the same allowlist as .github/actions/zig-fetch-retry/fetch.sh (ADR-0020 in the
# herdr-AGENT wrapper). A build.zig.zon fetch error must name an allowlisted cause at the
# end of its line. Every other error diagnostic, located or plain (a rustc or clippy error, a
# Zig compile error, a hash mismatch, a permanent fetch cause, a missing system library, a
# linker error, another crate's build script), fails at once, so a second run can never
# turn a code verdict green. The only other error line admitted is cargo's wrapper for the
# ghostty-vt build script, which is how the fetch failure reaches cargo's output.
# Offline suite: scripts/test_windows_check.sh.
$ZonFetchError = 'build\.zig\.zon:[0-9]+:[0-9]+: error: '
$TransientZigFetch = $ZonFetchError + '(unable to connect to server: (NameServerFailure|ConnectionRefused|ConnectionResetByPeer|HostUnreachable|NetworkUnreachable|NetworkDown|Timeout)' +
    '|HTTP request failed: WriteFailed' +
    '|HTTP response read failure: (ConnectionResetByPeer|NetworkDown|Timeout)' +
    "|bad HTTP response code: '(408|429|5[0-9][0-9])( [^']*)?')\s*$"
# Any error diagnostic: `error:` or `error[CODE]:` at the start of a line or after a space or
# a location colon.
$ErrorDiagnostic = '(^|[\s:])error(\[[A-Za-z0-9]+\])?: '
$GhosttyBuildScriptWrapper = '^error: failed to run custom build command for `ghostty-vt v[0-9][^`]*`\s*$'

function Test-TransientZigFetchFailure {
    param([string[]]$Lines)

    $transient = 0
    foreach ($line in $Lines) {
        if ($line -cmatch $TransientZigFetch) {
            $transient++
            continue
        }
        if ($line -cmatch $GhosttyBuildScriptWrapper) {
            continue
        }
        # Any other error diagnostic is a verdict, not a fetch failure.
        if ($line -cmatch $ErrorDiagnostic) {
            return $false
        }
    }
    return $transient -gt 0
}

function Invoke-CargoWithZigCacheRecovery {
    param([string[]]$Arguments)

    # Windows PowerShell turns redirected native stderr into error records; with
    # ErrorActionPreference=Stop the first one would abort the capture.
    $saved = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $output = @(& cargo @Arguments 2>&1 | ForEach-Object { "$_" })
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    $output | ForEach-Object { Write-Host $_ }
    if ($code -eq 0) {
        return
    }
    if (-not (Test-TransientZigFetchFailure $output)) {
        throw "command failed with exit code $code`: cargo $($Arguments -join ' ') (not a transient Zig package fetch; not retried)"
    }

    Write-Warning "cargo compile failed on a transient Zig package fetch; clearing Zig build caches and retrying once"
    Remove-Item -Recurse -Force .zig-cache -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force vendor/libghostty-vt/.zig-cache -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force vendor/libghostty-vt/zig-out -ErrorAction SilentlyContinue
    Invoke-Checked cargo $Arguments
}

Invoke-Checked cargo @("fmt", "--check")
Invoke-CargoWithZigCacheRecovery @(
    "clippy",
    "--all-targets",
    "--locked",
    "--",
    "-D",
    "warnings"
)

if ($Mode -eq "lint") {
    return
}

Invoke-Checked just @("test")
Invoke-Checked cargo @("build", "--locked")
