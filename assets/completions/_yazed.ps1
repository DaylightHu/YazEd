Register-ArgumentCompleter -Native -CommandName yazed -ScriptBlock {
	param($wordToComplete, $commandAst, $cursorPosition)

	$words = @(
		foreach ($element in $commandAst.CommandElements) {
			if ($element.Extent.StartOffset -ge $cursorPosition) { break }
			try { [string]$element.SafeGetValue() } catch { return }
		}
		if ($wordToComplete -eq '') { '' }
	)

	foreach ($cand in (& yazed --completion=powershell -- @words)) {
		$cand, $help = $cand.Split("`t", 2)
		if (-not $help) { $help = $cand }
		$text = "'" + $cand.Replace("'", "''") + "'"
		[System.Management.Automation.CompletionResult]::new($text, $cand, 'ParameterValue', $help)
	}
}
