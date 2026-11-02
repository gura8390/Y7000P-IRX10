[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [string]$Output,
    [switch]$AllowAdjacentDlls,
    [ValidateSet('requireAdministrator','asInvoker','any')][string]$ExpectedExecutionLevel = 'requireAdministrator'
)
# Static only: no LoadLibrary, process launch, device access or PATH resolution.
$ErrorActionPreference = 'Stop'
function Assert-Bytes($Image,[long]$Offset,[long]$Count) {
    if ($Offset -lt 0 -or $Count -lt 0 -or $Offset -gt $Image.Bytes.LongLength - $Count) { throw 'PE data exceeds file bounds.' }
}
function U16($Image,[long]$Offset) { Assert-Bytes $Image $Offset 2; [BitConverter]::ToUInt16($Image.Bytes,[int]$Offset) }
function U32($Image,[long]$Offset) { Assert-Bytes $Image $Offset 4; [BitConverter]::ToUInt32($Image.Bytes,[int]$Offset) }
function U64($Image,[long]$Offset) { Assert-Bytes $Image $Offset 8; [BitConverter]::ToUInt64($Image.Bytes,[int]$Offset) }
function Pe-Image([string]$Path) {
    $item = Get-Item -LiteralPath $Path
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $item.Length -gt 256MB) { throw 'Refusing an oversized or reparse-point image.' }
    $image = @{ Bytes=[IO.File]::ReadAllBytes($item.FullName); Sections=@() }
    if ((U16 $image 0) -ne 0x5a4d) { throw 'Missing DOS image signature.' }
    $pe = [long](U32 $image 0x3c)
    if ((U32 $image $pe) -ne 0x4550 -or (U16 $image ($pe+4)) -ne 0x8664) { throw 'Expected an AMD64 Windows PE image.' }
    $count = U16 $image ($pe+6); $optionalSize=U16 $image ($pe+20); $optional=$pe+24
    if ($count -gt 96 -or $optionalSize -lt 240 -or (U16 $image $optional) -ne 0x20b) { throw 'Invalid or unsupported PE32+ headers.' }
    Assert-Bytes $image $optional $optionalSize
    $image.Optional=$optional; $image.ImageBase=U64 $image ($optional+24)
    $image.HeaderSize=U32 $image ($optional+60); $image.Subsystem=U16 $image ($optional+68)
    $image.DirectoryCount=U32 $image ($optional+108)
    if ($image.DirectoryCount -gt [Math]::Floor(($optionalSize-112)/8)) { throw 'PE data directories exceed optional header.' }
    for ($index=0; $index -lt $count; $index++) {
        $section=$optional+$optionalSize+$index*40; Assert-Bytes $image $section 40
        $sectionName=[Text.Encoding]::ASCII.GetString($image.Bytes,[int]$section,8).TrimEnd([char]0)
        $image.Sections+=@{ Name=$sectionName; Rva=[long](U32 $image ($section+12)); Virtual=[long](U32 $image ($section+8)); RawSize=[long](U32 $image ($section+16)); Raw=[long](U32 $image ($section+20)) }
    }
    return $image
}
function Pe-Offset($Image,[long]$Rva,[long]$Count=1) {
    if ($Rva -lt 0 -or $Count -lt 0) { throw 'Invalid PE RVA.' }
    if ($Rva -lt $Image.HeaderSize -and $Rva+$Count -le $Image.HeaderSize) { Assert-Bytes $Image $Rva $Count; return $Rva }
    foreach ($section in $Image.Sections) {
        $relative=$Rva-$section.Rva
        if ($relative -ge 0 -and $relative+$Count -le $section.RawSize) { $offset=$section.Raw+$relative; Assert-Bytes $Image $offset $Count; return $offset }
    }
    throw 'PE RVA has no file-backed section.'
}
function Pe-Directory($Image,[int]$Index) {
    if ($Index -ge $Image.DirectoryCount) { return @{Rva=0L;Size=0L} }
    $offset=$Image.Optional+112+$Index*8
    return @{ Rva=[long](U32 $Image $offset); Size=[long](U32 $Image ($offset+4)) }
}
function Pe-Ascii($Image,[long]$Rva) {
    $builder=[Text.StringBuilder]::new()
    for ($length=0; $length -lt 260; $length++) {
        $offset=Pe-Offset $Image ($Rva+$length)
        $byte=$Image.Bytes[$offset]; if ($byte -eq 0) { return $builder.ToString() }
        if ($byte -lt 32 -or $byte -gt 126) { throw 'Non-ASCII PE import name.' }
        [void]$builder.Append([char]$byte)
    }
    throw 'Unterminated PE import name.'
}
function Pe-Imports($Image) {
    $names=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($table in @(@{Index=1;Stride=20;Name=12},@{Index=13;Stride=32;Name=4})) {
        $directory=Pe-Directory $Image $table.Index
        if ($directory.Rva -eq 0 -and $directory.Size -eq 0) { continue }
        if ($directory.Rva -eq 0 -or $directory.Size -lt $table.Stride) { throw 'Malformed import directory.' }
        $terminated=$false
        for ($index=0; $index -lt [Math]::Min(4096,[Math]::Floor($directory.Size/$table.Stride)); $index++) {
            $offset=Pe-Offset $Image ($directory.Rva+$index*$table.Stride) $table.Stride
            $empty=$true
            for($field=0;$field -lt $table.Stride;$field+=4){if((U32 $Image ($offset+$field)) -ne 0){$empty=$false}}
            if($empty){$terminated=$true;break}
            $nameRva=[long](U32 $Image ($offset+$table.Name))
            if ($table.Index -eq 13 -and ((U32 $Image $offset) -band 1) -eq 0) {
                if ([uint64]$nameRva -lt $Image.ImageBase) { throw 'Unsupported absolute delay-import address.' }
                $nameRva=[long]([uint64]$nameRva-$Image.ImageBase)
            }
            $name=Pe-Ascii $Image $nameRva
            if ($name -notmatch '^[A-Za-z0-9_.-]+\.dll$') { throw 'Unsafe or unsupported PE import name.' }
            [void]$names.Add($name)
        }
        if (!$terminated) { throw 'Import directory has no bounded terminator.' }
    }
    return @($names | Sort-Object)
}
$script:ApiSetContracts=$null
$script:ApiSetPrefixes=$null
function ApiSet-Hosts([string]$Name) {
    if ($null -eq $script:ApiSetContracts) {
        # Read the actual OS namespace; a name prefix alone does not prove a contract exists.
        $schema=Pe-Image (Join-Path ([Environment]::SystemDirectory) 'apisetschema.dll')
        $section=@($schema.Sections | Where-Object {$_.Name -ceq '.apiset'})
        if($section.Count -ne 1){throw 'Windows API-set schema section is unavailable.'}
        $base=$section[0].Raw; $capacity=$section[0].RawSize
        if((U32 $schema $base) -ne 6){throw 'Unsupported Windows API-set namespace version; use an independent loader check.'}
        $size=[long](U32 $schema ($base+4));$count=[long](U32 $schema ($base+12));$entries=[long](U32 $schema ($base+16))
        if($size -gt $capacity -or $count -gt 65536 -or $entries+$count*24 -gt $size){throw 'Invalid Windows API-set namespace bounds.'}
        $contracts=@{};$prefixes=@{}
        for($index=0;$index -lt $count;$index++){
            $entry=$base+$entries+$index*24;$nameOffset=[long](U32 $schema ($entry+4));$nameLength=[long](U32 $schema ($entry+8));$hashedLength=[long](U32 $schema ($entry+12));$valueOffset=[long](U32 $schema ($entry+16));$valueCount=[long](U32 $schema ($entry+20))
            if($nameOffset+$nameLength -gt $size -or $nameLength%2 -ne 0 -or $hashedLength -gt $nameLength -or $hashedLength%2 -ne 0 -or $valueCount -gt 256 -or $valueOffset+$valueCount*20 -gt $size){throw 'Invalid Windows API-set entry.'}
            $stem=[Text.Encoding]::Unicode.GetString($schema.Bytes,[int]($base+$nameOffset),[int]$nameLength);$contract=$stem+'.dll'
            $hosts=@();$defaults=@()
            for($valueIndex=0;$valueIndex -lt $valueCount;$valueIndex++){
                $value=$base+$valueOffset+$valueIndex*20;$aliasLength=U32 $schema ($value+8);$hostOffset=[long](U32 $schema ($value+12));$hostLength=[long](U32 $schema ($value+16))
                if($hostOffset+$hostLength -gt $size -or $hostLength%2 -ne 0){throw 'Invalid Windows API-set host.'}
                $hostName=[Text.Encoding]::Unicode.GetString($schema.Bytes,[int]($base+$hostOffset),[int]$hostLength)
                if($hostName){if($hostName -notmatch '^[A-Za-z0-9_.-]+$'){throw ('Invalid Windows API-set host name: '+$hostName)};$hosts+=$hostName;if($aliasLength -eq 0){$defaults+=$hostName}}
            }
            $contracts[$contract]=if($defaults.Count){@($defaults|Sort-Object -Unique)}else{@($hosts|Sort-Object -Unique)}
            if($hashedLength -gt 0){$prefix=$stem.Substring(0,[int]($hashedLength/2));$prefixes[$prefix]=@(@($prefixes[$prefix]) + @($contracts[$contract]) | Where-Object{$_} | Sort-Object -Unique)}
        }
        $script:ApiSetContracts=$contracts;$script:ApiSetPrefixes=$prefixes
    }
    $resolved=@($script:ApiSetContracts[$Name])
    if(!$resolved.Count -or !$resolved[0]){
        # API-set schema v6 hashes the contract excluding its trailing revision.
        # Match the OS-provided HashedLength prefix rather than inventing a version map.
        $stem=$Name -replace '\.dll$',''
        $prefix=$stem -replace '-[0-9]+$',''
        $resolved=@($script:ApiSetPrefixes[$prefix])
    }
    foreach($hostName in $resolved){if($hostName){$hostName}}
}
function Resource-Children($Image,[long]$Root,[long]$Size,[long]$Relative,[int]$Depth=0) {
    if ($Depth -gt 4 -or $Relative -lt 0 -or $Relative -gt $Size-16) { throw 'Malformed resource tree.' }
    $offset=$Root+$Relative; Assert-Bytes $Image $offset 16
    $count=(U16 $Image ($offset+12))+(U16 $Image ($offset+14))
    if ($count -gt 4096 -or $Relative+16+$count*8 -gt $Size) { throw 'Resource entries exceed directory.' }
    for($index=0;$index -lt $count;$index++) {
        $entry=$offset+16+$index*8; $name=U32 $Image $entry; $target=U32 $Image ($entry+4)
        if ($Depth -eq 0 -and $name -ne 24) { continue }
        $relativeTarget=[long]($target -band 0x7fffffff)
        if (($target -band 0x80000000L) -ne 0) { Resource-Children $Image $Root $Size $relativeTarget ($Depth+1) }
        else {
            if($relativeTarget -gt $Size-16){throw 'Resource data entry exceeds directory.'}
            $data=$Root+$relativeTarget; $rva=[long](U32 $Image $data);$length=[long](U32 $Image ($data+4))
            if($length -gt 1MB){throw 'Oversized application manifest.'}
            $body=Pe-Offset $Image $rva $length
            [Text.Encoding]::UTF8.GetString($Image.Bytes,[int]$body,[int]$length)
        }
    }
}
$binaryPath=(Resolve-Path -LiteralPath $Binary).Path
$packageRoot=[IO.Path]::GetDirectoryName($binaryPath)
$image=Pe-Image $binaryPath
if($image.Subsystem -ne 2){throw 'Expected a GUI subsystem application.'}
$resource=Pe-Directory $image 2
if($resource.Rva -eq 0){throw 'Application manifest resource is absent.'}
$manifests=@(Resource-Children $image (Pe-Offset $image $resource.Rva $resource.Size) $resource.Size 0)
$levels=@($manifests | ForEach-Object { if($_ -match '<requestedExecutionLevel\b[^>]*\blevel="(asInvoker|requireAdministrator|highestAvailable)"'){ $Matches[1] } } | Select-Object -Unique)
if($levels.Count -ne 1){throw 'Expected exactly one consistent requested-execution level.'}
if($ExpectedExecutionLevel -ne 'any' -and $levels[0] -cne $ExpectedExecutionLevel){throw 'The actual resource manifest has an unexpected execution level.'}
$direct=@(Pe-Imports $image)
$queue=[Collections.Generic.Queue[string]]::new();foreach($name in $direct){$queue.Enqueue($name)}
$visited=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$closure=@();$missing=@()
while($queue.Count){
    $name=$queue.Dequeue();if(!$visited.Add($name)){continue}
    $packaged=Join-Path $packageRoot $name
    if((Test-Path -LiteralPath $packaged -PathType Leaf) -and !$AllowAdjacentDlls){
        $missing+=$name;$closure+=[ordered]@{name=$name;resolved='adjacent DLL is excluded by the single-executable package allowlist'}
    }elseif(Test-Path -LiteralPath $packaged -PathType Leaf){
        $dependency=Pe-Image $packaged;foreach($child in @(Pe-Imports $dependency)){$queue.Enqueue($child)}
        $closure+= [ordered]@{name=$name;resolved='package';sha256=(Get-FileHash -LiteralPath $packaged -Algorithm SHA256).Hash.ToLowerInvariant()}
    }elseif($name -match '^(api-ms-win-|ext-ms-win-).+\.dll$'){
        $hosts=@(ApiSet-Hosts $name)
        $absent=@($hosts|Where-Object{!(Test-Path -LiteralPath (Join-Path ([Environment]::SystemDirectory) $_) -PathType Leaf)})
        if(!$hosts.Count -or $absent.Count){$missing+=$name;$closure+=[ordered]@{name=$name;resolved='missing Windows API-set contract or host';hosts=$hosts}}
        else{$closure+=[ordered]@{name=$name;resolved='Windows API-set namespace and system hosts';hosts=$hosts}}
    }elseif(Test-Path -LiteralPath (Join-Path ([Environment]::SystemDirectory) $name) -PathType Leaf){
        $closure+= [ordered]@{name=$name;resolved='Windows system directory'}
    }else{
        $missing+=$name;$closure+=[ordered]@{name=$name;resolved='missing (PATH deliberately ignored)'}
    }
}
$report=[ordered]@{
    schema=1;audited_utc=[DateTime]::UtcNow.ToString('o');scope='Static PE imports and recursive packaged-DLL closure only; no image was executed. OS/API-set transitive dependencies are serviced by Windows. Optional runtime LoadLibrary dependencies require a separate source/capability audit.'
    adjacent_dlls_allowed=[bool]$AllowAdjacentDlls;binary=[IO.Path]::GetFileName($binaryPath);bytes=(Get-Item -LiteralPath $binaryPath).Length;sha256=(Get-FileHash -LiteralPath $binaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
    machine='AMD64';subsystem='Windows GUI';execution_level=$levels[0];imports=$direct;closure=$closure;missing=$missing;verified=($missing.Count -eq 0)
}
$json=$report | ConvertTo-Json -Depth 7
if($Output){$destination=[IO.Path]::GetFullPath($Output);if(Test-Path -LiteralPath $destination){throw 'Do not overwrite an existing PE audit.'};[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destination))|Out-Null;[IO.File]::WriteAllText($destination,$json,[Text.UTF8Encoding]::new($false))}
$json
if($missing.Count){throw ('Unresolved import dependencies: '+($missing -join ', '))}
