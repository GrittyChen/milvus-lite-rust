#Requires -Version 7.2
param([Parameter(Mandatory=$true)][string]$WorkDirectory)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$work = [IO.Path]::GetFullPath($WorkDirectory)
if ($work.StartsWith($repo + [IO.Path]::DirectorySeparatorChar) -or $work -eq $repo -or (Test-Path $work)) {
    throw '工作目录须为仓库外的新目录'
}
New-Item -ItemType Directory -Path $work | Out-Null
Push-Location $repo
try {
    foreach ($feature in @('', 'fault-injection')) {
        foreach ($command in @('check', 'clippy')) {
            $arguments = @($command, '--locked', '-p', 'milvus-lite-core', '-p', 'milvus-lite-grpc', '--target', 'x86_64-pc-windows-msvc', '--all-targets')
            if ($feature) { $arguments += @('--features', $feature) }
            if ($command -eq 'clippy') { $arguments += @('--', '-D', 'warnings') }
            & cargo @arguments *> (Join-Path $work "$command-$feature.log")
            if ($LASTEXITCODE -ne 0) { throw "静态门槛失败：$command $feature" }
        }
    }
    & cargo check --locked -p milvus-lite-py --target x86_64-pc-windows-msvc *> (Join-Path $work 'python-rejection.log')
    $code = $LASTEXITCODE
    $message = Get-Content -Raw (Join-Path $work 'python-rejection.log')
    if ($code -eq 0 -or !$message.Contains('milvus-lite-rust build rejected: Windows is not supported in this release')) {
        throw '未获得本项目预期的 Windows 构建拒绝，不能用其它编译错误代替'
    }
    Write-Output 'Windows 静态准备及构建拒绝检查完成；没有 Windows 运行支持结论'
} finally {
    Pop-Location
}
