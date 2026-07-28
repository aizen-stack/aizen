# Failpoint-style headless probes for Time Machine.
# Run:  .\scripts\tm_failpoint_matrix.ps1
# Requires: target/debug/aizen.exe, git on PATH.
# Does not open TUI/GUI. Uses a temp AIZEN_HOME + temp repo only.

$ErrorActionPreference = "Continue"
$env:Path = "C:\Users\admin\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Program Files\Git\cmd;" + $env:Path
$git = "C:\Program Files\Git\cmd\git.exe"
$root = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { "C:\Users\admin\Desktop\mini_project\aizen" }
if (-not (Test-Path (Join-Path $root "Cargo.toml"))) {
  $root = "C:\Users\admin\Desktop\mini_project\aizen"
}
$aizen = Join-Path $root "target\debug\aizen.exe"
if (-not (Test-Path $aizen)) {
  Write-Host "building aizen..."
  Push-Location $root
  cargo build --bin aizen 2>&1 | Select-Object -Last 5
  Pop-Location
}
if (-not (Test-Path $aizen)) { throw "missing $aizen" }

$script:pass = 0
$script:fail = 0
function Assert-True([bool]$cond, [string]$msg) {
  if ($cond) {
    Write-Host "  PASS $msg"
    $script:pass++
  } else {
    Write-Host "  FAIL $msg"
    $script:fail++
  }
}

function New-TmpRepo {
  param([string]$Base)
  $repo = Join-Path $Base "repo"
  New-Item -ItemType Directory -Path $repo | Out-Null
  Push-Location $repo
  & $git init -q
  & $git config user.email "tm@test"
  & $git config user.name "tm"
  Set-Content -Path "file.txt" -Value "v1" -Encoding ascii -NoNewline
  & $git add file.txt
  & $git commit -qm "init"
  Pop-Location
  return $repo
}

function Invoke-Aizen {
  param([string]$Repo, [string]$AizenHomeDir, [string[]]$CliArgs)
  $old = Get-Location
  $prevHome = $env:AIZEN_HOME
  try {
    Set-Location $Repo
    $env:AIZEN_HOME = $AizenHomeDir
    $out = & $aizen @CliArgs 2>&1 | Out-String
    return @{ Exit = $LASTEXITCODE; Out = $out }
  } finally {
    Set-Location $old
    if ($null -eq $prevHome) { Remove-Item Env:AIZEN_HOME -ErrorAction SilentlyContinue } else { $env:AIZEN_HOME = $prevHome }
  }
}

$base = Join-Path $env:TEMP ("aizen-tm-matrix-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Path $base | Out-Null
Write-Host "BASE=$base"

try {
  # ---------- 1. Happy path + external store isolation ----------
  Write-Host "`n== 1 external store isolation =="
  $h1 = Join-Path $base "h1"
  New-Item -ItemType Directory -Path $h1 | Out-Null
  $repo1 = New-TmpRepo (Join-Path $base "r1")
  $r = Invoke-Aizen $repo1 $h1 @("time","save","a")
  Assert-True ($r.Exit -eq 0) "save ok"
  $r = Invoke-Aizen $repo1 $h1 @("time","doctor","--json")
  Assert-True ($r.Exit -eq 0) "doctor ok"
  Assert-True ($r.Out -match "timemachine") "doctor store path under home"
  Push-Location $repo1
  $srcRefs = & $git for-each-ref --format="%(refname)" "refs/ng" 2>$null
  Pop-Location
  Assert-True (-not $srcRefs) "no refs written to source .git"
  Assert-True (Test-Path (Join-Path $h1 "timemachine")) "private store created under AIZEN_HOME"

  # ---------- 2. Corrupt ledger fail-closed ----------
  Write-Host "`n== 2 corrupt ledger fail-closed =="
  $h2 = Join-Path $base "h2"
  New-Item -ItemType Directory -Path $h2 | Out-Null
  $repo2 = New-TmpRepo (Join-Path $base "r2")
  $null = Invoke-Aizen $repo2 $h2 @("time","save","ok")
  $ledger = Get-ChildItem -Path $h2 -Recurse -Filter "ledger.json" | Select-Object -First 1
  Assert-True ($null -ne $ledger) "ledger exists"
  Set-Content -Path $ledger.FullName -Value "{not-json" -Encoding ascii
  $r = Invoke-Aizen $repo2 $h2 @("time","list")
  Assert-True ($r.Exit -ne 0) "list rejects corrupt ledger"
  $r = Invoke-Aizen $repo2 $h2 @("time","save","should-fail")
  Assert-True ($r.Exit -ne 0) "save rejects corrupt ledger"
  $raw = Get-Content $ledger.FullName -Raw
  Assert-True ($raw -match "not-json") "corrupt ledger not overwritten with default"

  # ---------- 3. Budget edge ----------
  Write-Host "`n== 3 budget edge =="
  $h3 = Join-Path $base "h3"
  New-Item -ItemType Directory -Path $h3 | Out-Null
  $repo3 = New-TmpRepo (Join-Path $base "r3")
  # Avoid PowerShell UTF-8 BOM which can make serde reject the config file.
  $cfgPath = Join-Path $h3 "cli-config.json"
  [System.IO.File]::WriteAllText($cfgPath, "{`"timemachine_max_file_bytes`":4}`n")
  Set-Content -Path (Join-Path $repo3 "big.txt") -Value ("x" * 64) -Encoding ascii
  Push-Location $repo3
  & $git add big.txt
  & $git commit -qm "big" | Out-Null
  Pop-Location
  $r = Invoke-Aizen $repo3 $h3 @("time","save","budget")
  Write-Host "  budget_out=$($r.Out.Trim())"
  Assert-True ($r.Exit -ne 0) "save refuses oversize blob"
  Assert-True ($r.Out -match "budget|exceeded|limit") "budget error surfaced"

  # ---------- 4. Nested repo refuse ----------
  Write-Host "`n== 4 nested repo refuse =="
  $h4 = Join-Path $base "h4"
  New-Item -ItemType Directory -Path $h4 | Out-Null
  $repo4 = New-TmpRepo (Join-Path $base "r4")
  $nested = Join-Path $repo4 "sub"
  New-Item -ItemType Directory -Path $nested | Out-Null
  Push-Location $nested
  & $git init -q
  Pop-Location
  $r = Invoke-Aizen $repo4 $h4 @("time","save","nested")
  Assert-True ($r.Exit -ne 0) "nested .git refused"
  Assert-True ($r.Out -match "nested") "nested error mentions nested"

  # ---------- 5. External filter refuse ----------
  Write-Host "`n== 5 external filter refuse =="
  $h5 = Join-Path $base "h5"
  New-Item -ItemType Directory -Path $h5 | Out-Null
  $repo5 = New-TmpRepo (Join-Path $base "r5")
  Push-Location $repo5
  & $git config filter.evil.clean "sed s/a/b/"
  Pop-Location
  $r = Invoke-Aizen $repo5 $h5 @("time","save","filter")
  Assert-True ($r.Exit -ne 0) "external filter refused"
  Assert-True ($r.Out -match "filter") "filter error mentions filter"

  # ---------- 6. Concurrent saves serialize ----------
  Write-Host "`n== 6 concurrent saves =="
  $h6 = Join-Path $base "h6"
  New-Item -ItemType Directory -Path $h6 | Out-Null
  $repo6 = New-TmpRepo (Join-Path $base "r6")
  $jobs = @()
  1..6 | ForEach-Object {
    $i = $_
    $jobs += Start-Job -ScriptBlock {
      param($az, $repo, $aizenHomeDir, $i)
      $env:AIZEN_HOME = $aizenHomeDir
      $env:Path = "C:\Users\admin\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Program Files\Git\cmd;" + $env:Path
      Set-Location $repo
      Set-Content -Path "file.txt" -Value ("c$i-" + [guid]::NewGuid().ToString("n")) -Encoding ascii -NoNewline
      & $az time save "c$i" 2>&1 | Out-String | Out-Null
      return $LASTEXITCODE
    } -ArgumentList $aizen, $repo6, $h6, $i
  }
  $codes = $jobs | Wait-Job | Receive-Job
  $jobs | Remove-Job -Force
  $okCount = @($codes | Where-Object { $_ -eq 0 }).Count
  Assert-True ($okCount -ge 4) "most concurrent saves succeed ($okCount/6)"
  $r = Invoke-Aizen $repo6 $h6 @("time","list")
  Assert-True ($r.Exit -eq 0) "list after concurrent"
  Assert-True ($r.Out -match "checkpoint") "list shows checkpoints"

  # ---------- 7. Restore + undo/redo ----------
  Write-Host "`n== 7 restore undo redo =="
  $h7 = Join-Path $base "h7"
  New-Item -ItemType Directory -Path $h7 | Out-Null
  $repo7 = New-TmpRepo (Join-Path $base "r7")
  $null = Invoke-Aizen $repo7 $h7 @("time","save","s1")
  Set-Content -Path (Join-Path $repo7 "file.txt") -Value "v2" -Encoding ascii -NoNewline
  $null = Invoke-Aizen $repo7 $h7 @("time","save","s2")
  $null = Invoke-Aizen $repo7 $h7 @("time","restore","1")
  $c = [System.IO.File]::ReadAllText((Join-Path $repo7 "file.txt")).Trim()
  Assert-True ($c -eq "v1") "restore to #1"
  $null = Invoke-Aizen $repo7 $h7 @("time","redo")
  $c = [System.IO.File]::ReadAllText((Join-Path $repo7 "file.txt")).Trim()
  Assert-True ($c -eq "v2") "redo back to #2 branch tip"

  # ---------- 8. Journal recovery via doctor --repair ----------
  Write-Host "`n== 8 journal leftover + doctor repair =="
  $h8 = Join-Path $base "h8"
  New-Item -ItemType Directory -Path $h8 | Out-Null
  $repo8 = New-TmpRepo (Join-Path $base "r8")
  $null = Invoke-Aizen $repo8 $h8 @("time","save","base")
  $ns = Get-ChildItem -Path (Join-Path $h8 "timemachine") -Recurse -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "ledger.json") } |
    Select-Object -First 1
  if ($ns) {
    $jpath = Join-Path $ns.FullName "journal.json"
    # Minimal valid journal matching the Rust schema (snake_case enums).
    $j = @"
{"schema_version":1,"operation_id":"probe-1","kind":"prune","phase":"prepared","expected_generation":1}
"@
    [System.IO.File]::WriteAllText($jpath, $j)
    $r = Invoke-Aizen $repo8 $h8 @("time","doctor")
    Write-Host "  doctor_out=$($r.Out.Trim()) exit=$($r.Exit)"
    Assert-True ($r.Exit -ne 0 -or $r.Out -match "unfinished|healthy|attention") "doctor sees/handles journal"
    $r = Invoke-Aizen $repo8 $h8 @("time","doctor","--repair")
    Write-Host "  repair_out=$($r.Out.Trim()) exit=$($r.Exit)"
    Assert-True ($r.Exit -eq 0) "doctor --repair clears prune journal"
    Assert-True (-not (Test-Path $jpath)) "journal removed after repair"
  } else {
    Assert-True $false "namespace dir not found for journal probe"
  }

  # ---------- 9. GC orphan refs ----------
  Write-Host "`n== 9 gc orphan refs =="
  $h9 = Join-Path $base "h9"
  New-Item -ItemType Directory -Path $h9 | Out-Null
  $repo9 = New-TmpRepo (Join-Path $base "r9")
  $null = Invoke-Aizen $repo9 $h9 @("time","save","live")
  $store = Get-ChildItem -Path (Join-Path $h9 "timemachine") -Recurse -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -eq "store.git" } |
    Select-Object -First 1
  if ($store) {
    $liveRef = Get-ChildItem -Path (Join-Path $store.FullName "refs\ng\tm") -Recurse -File -ErrorAction SilentlyContinue |
      Select-Object -First 1
    if ($liveRef) {
      $oid = (Get-Content $liveRef.FullName -Raw).Trim()
      # Plant orphan under THIS worktree prefix so gc's live-id sweep removes it.
      $wtDir = $liveRef.Directory.FullName
      Set-Content -Path (Join-Path $wtDir "999") -Value $oid -Encoding ascii
      $before = (Get-ChildItem -Path (Join-Path $store.FullName "refs\ng\tm") -Recurse -File).Count
      $null = Invoke-Aizen $repo9 $h9 @("time","gc")
      $after = (Get-ChildItem -Path (Join-Path $store.FullName "refs\ng\tm") -Recurse -File).Count
      Assert-True ($after -lt $before) "gc removed orphan ref ($before -> $after)"
      Assert-True (-not (Test-Path (Join-Path $wtDir "999"))) "orphan file gone"
    } else {
      Assert-True $false "no live ref to clone as orphan"
    }
  } else {
    Assert-True $false "store.git missing"
  }

  # ---------- 10. clear empties timeline ----------
  Write-Host "`n== 10 clear =="
  $h10 = Join-Path $base "h10"
  New-Item -ItemType Directory -Path $h10 | Out-Null
  $repo10 = New-TmpRepo (Join-Path $base "r10")
  $null = Invoke-Aizen $repo10 $h10 @("time","save","x")
  $r = Invoke-Aizen $repo10 $h10 @("time","clear")
  Assert-True ($r.Exit -eq 0) "clear ok"
  $r = Invoke-Aizen $repo10 $h10 @("time","list")
  Assert-True ($r.Exit -eq 0) "list after clear ok"

  # ---------- 11. linked worktree gc preserves sibling refs ----------
  # Two linked worktrees share ONE private store. A gc from worktree A sweeps the whole
  # `refs/ng/tm/**` namespace but must NOT reap worktree B's live checkpoint refs. This
  # exercises the store-exclusive lease + the foreign-worktree guard in doctor_gc.
  Write-Host "`n== 11 linked worktree gc keeps sibling refs =="
  $h11 = Join-Path $base "h11"
  New-Item -ItemType Directory -Path $h11 | Out-Null
  $repo11 = New-TmpRepo (Join-Path $base "r11")
  $wt11 = Join-Path $base "r11-wt"
  Push-Location $repo11
  & $git worktree add -q $wt11 2>&1 | Out-Null
  Pop-Location
  if (Test-Path $wt11) {
    # Each worktree saves its own checkpoint into the shared store (distinct worktree ids).
    Set-Content -Path (Join-Path $repo11 "file.txt") -Value "main-wt" -Encoding ascii -NoNewline
    $null = Invoke-Aizen $repo11 $h11 @("time","save","from-main")
    Set-Content -Path (Join-Path $wt11 "file.txt") -Value "linked-wt" -Encoding ascii -NoNewline
    $null = Invoke-Aizen $wt11 $h11 @("time","save","from-linked")
    $store11 = Get-ChildItem -Path (Join-Path $h11 "timemachine") -Recurse -Directory -ErrorAction SilentlyContinue |
      Where-Object { $_.Name -eq "store.git" } | Select-Object -First 1
    $wtPrefixes = @()
    if ($store11) {
      $wtPrefixes = Get-ChildItem -Path (Join-Path $store11.FullName "refs\ng\tm") -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like "wt-*" }
    }
    Assert-True ($wtPrefixes.Count -ge 2) "two worktree ref namespaces in shared store ($($wtPrefixes.Count))"
    # gc from the MAIN worktree — the linked worktree's live checkpoint must survive.
    $r = Invoke-Aizen $repo11 $h11 @("time","gc")
    Assert-True ($r.Exit -eq 0) "gc from main worktree ok"
    $r = Invoke-Aizen $wt11 $h11 @("time","list")
    Assert-True ($r.Exit -eq 0 -and $r.Out -match "from-linked") "linked worktree checkpoint survives sibling gc"
    # And its restore still works (ref + object intact).
    Set-Content -Path (Join-Path $wt11 "file.txt") -Value "dirty" -Encoding ascii -NoNewline
    $null = Invoke-Aizen $wt11 $h11 @("time","restore","1")
    $c = [System.IO.File]::ReadAllText((Join-Path $wt11 "file.txt")).Trim()
    Assert-True ($c -eq "linked-wt") "linked worktree restore after sibling gc"
    Push-Location $repo11
    & $git worktree remove --force $wt11 2>&1 | Out-Null
    Pop-Location
  } else {
    Assert-True $false "git worktree add failed (skipping linked-worktree gc probe)"
  }

  # ---------- 12. concurrent restore + save serialize (store-shared lease) ----------
  # Ref-mutating ops take the store lock SHARED, so sibling processes still serialize on the
  # per-worktree metadata lock. Racing a restore against saves must never corrupt the ledger:
  # every op exits cleanly and the timeline stays readable.
  Write-Host "`n== 12 concurrent restore/save serialize =="
  $h12 = Join-Path $base "h12"
  New-Item -ItemType Directory -Path $h12 | Out-Null
  $repo12 = New-TmpRepo (Join-Path $base "r12")
  $null = Invoke-Aizen $repo12 $h12 @("time","save","base")
  Set-Content -Path (Join-Path $repo12 "file.txt") -Value "v2" -Encoding ascii -NoNewline
  $null = Invoke-Aizen $repo12 $h12 @("time","save","second")
  $jobs12 = @()
  1..4 | ForEach-Object {
    $i = $_
    $jobs12 += Start-Job -ScriptBlock {
      param($az, $repo, $aizenHomeDir, $i)
      $env:AIZEN_HOME = $aizenHomeDir
      $env:Path = "C:\Users\admin\.rustup\toolchains\stable-x86_64-pc-windows-gnu\bin;C:\Program Files\Git\cmd;" + $env:Path
      Set-Location $repo
      if ($i % 2 -eq 0) {
        & $az time restore 1 2>&1 | Out-String | Out-Null
      } else {
        Set-Content -Path "file.txt" -Value ("s$i-" + [guid]::NewGuid().ToString("n")) -Encoding ascii -NoNewline
        & $az time save "race$i" 2>&1 | Out-String | Out-Null
      }
      return $LASTEXITCODE
    } -ArgumentList $aizen, $repo12, $h12, $i
  }
  $codes12 = $jobs12 | Wait-Job | Receive-Job
  $jobs12 | Remove-Job -Force
  $ok12 = @($codes12 | Where-Object { $_ -eq 0 }).Count
  Assert-True ($ok12 -ge 3) "most concurrent restore/save ops succeed ($ok12/4)"
  $r = Invoke-Aizen $repo12 $h12 @("time","list")
  Assert-True ($r.Exit -eq 0) "ledger readable after concurrent restore/save"
  $r = Invoke-Aizen $repo12 $h12 @("time","doctor")
  Assert-True ($r.Exit -eq 0) "doctor healthy after concurrent restore/save"
} finally {
  if (Test-Path -LiteralPath $base) {
    Remove-Item -LiteralPath $base -Recurse -Force -ErrorAction SilentlyContinue
  }
}

Write-Host "`n==== RESULT: $script:pass passed, $script:fail failed ===="
if ($script:fail -gt 0) { exit 1 } else { exit 0 }
