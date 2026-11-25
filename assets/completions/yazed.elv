var yazed-completing = $false
var yazed-matcher = $edit:match-prefix~
if (has-key $edit:completion:matcher argument) {
	set yazed-matcher = $edit:completion:matcher[argument]
} elif (has-key $edit:completion:matcher '') {
	set yazed-matcher = $edit:completion:matcher['']
}

set edit:completion:matcher[argument] = {|seed|
	if $yazed-completing {
		set yazed-completing = $false
		each {|_| put $true }
	} else {
		$yazed-matcher $seed
	}
}

set edit:completion:arg-completer[yazed] = {|@words|
	var cands = [(yazed --completion=elvish -- $@words | from-lines)]
	put $@cands
	set yazed-completing = (> (count $cands) 0)
}
