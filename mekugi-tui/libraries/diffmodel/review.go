package diffmodel

// Source: review.go:1:256@[cc24b102] captured diff model

import (
	"fmt"
	"strings"

	"github.com/pmezard/go-difflib/difflib"
)

// ReviewFile is an immutable original-to-final review projection. Unlike the
// executor patch it includes deleted content and preserves line-ending changes.
// Paths are empty on the absent side of an addition or deletion.
type ReviewFile struct {
	BeforePath string
	AfterPath  string
	Diff       string
	// Incomplete describes unavailable content; it is not an empty-file diff.
	Incomplete string `json:",omitzero"`
	// Directory is a metadata-only directory change. Its contents and descendant
	// paths are not retained and cannot be composed, reverted or reapplied.
	Directory bool `json:",omitzero"`
	// Binary marks intact non-text content, reviewed by size and hash only.
	Binary bool `json:",omitzero"`
	// Link marks a symbolic link on either side; its rows are link targets,
	// not file content.
	Link bool `json:",omitzero"`
	// CopyFrom names an unchanged source whose content an added file copies.
	CopyFrom string `json:",omitzero"`
	// Origin labels the tool that chose this file or its content. Empty means
	// the agent chose both.
	Origin     string `json:",omitzero"`
	OriginNote string `json:",omitzero"`
}

// ReviewAction classifies a committed before/after file identity. Renderers use
// this one classification instead of inferring operation labels independently.
type ReviewAction string

const (
	ReviewAdd    ReviewAction = "add"
	ReviewUpdate ReviewAction = "update"
	ReviewDelete ReviewAction = "delete"
	ReviewMove   ReviewAction = "move"
)

// Action classifies the committed file operation from its captured identities.
func (file ReviewFile) Action() ReviewAction {
	switch {
	case file.BeforePath == "":
		return ReviewAdd
	case file.AfterPath == "":
		return ReviewDelete
	case file.BeforePath != file.AfterPath:
		return ReviewMove
	default:
		return ReviewUpdate
	}
}

// Title returns the user-facing operation name for reports and commentary.
func (action ReviewAction) Title() string {
	switch action {
	case ReviewAdd:
		return "Create"
	case ReviewDelete:
		return "Delete"
	case ReviewMove:
		return "Move"
	default:
		return "Edit"
	}
}

// UnifiedDiff omits the redundant operation header when unified headers already
// describe the file. Header-only changes (empty files and pure moves) retain it.
// Rendering captured records here also keeps historical reads consistent.
func (file ReviewFile) UnifiedDiff() string {
	_, rest, ok := strings.Cut(file.Diff, "\n")
	header := fmt.Sprintf("--- %s\n+++ %s\n", reviewPath(file.BeforePath), reviewPath(file.AfterPath))
	if ok && strings.HasPrefix(rest, header) {
		return rest
	}
	return file.Diff
}

// LineCounts counts added and removed source rows in a captured review projection.
// File headers, context, and missing-final-newline markers are not source changes.
// Incomplete captures return -1 for both counts.
func (file ReviewFile) LineCounts() (added, removed int) {
	if file.Incomplete != "" || file.Directory {
		return -1, -1 // Unknown, not zero changed rows.
	}
	inHunk := false
	for line := range strings.SplitSeq(file.Diff, "\n") {
		if line == "@@" || strings.HasPrefix(line, "@@ ") {
			inHunk = true
		} else if inHunk && strings.HasPrefix(line, "+") {
			added++
		} else if inHunk && strings.HasPrefix(line, "-") {
			removed++
		}
	}
	return added, removed
}

// renderReviewFile also renders sparse composed regions, without pretending that
// uncaptured source outside a region is known.
func renderReviewFile(file ReviewFile, a, b []string, beforeOffset, afterOffset int) ReviewFile {
	return renderReviewGroups(file, a, b, beforeOffset, afterOffset, difflib.NewMatcher(a, b).GetGroupedOpCodes(3))
}

func renderReviewGroups(file ReviewFile, a, b []string, beforeOffset, afterOffset int, groups [][]difflib.OpCode) ReviewFile {
	var diff strings.Builder
	fmt.Fprintf(&diff, "%s %q -> %q\n", file.Action(), file.BeforePath, file.AfterPath)
	if len(groups) != 0 {
		fmt.Fprintf(&diff, "--- %s\n+++ %s\n", reviewPath(file.BeforePath), reviewPath(file.AfterPath))
	}
	for _, group := range groups {
		first, last := group[0], group[len(group)-1]
		fmt.Fprintf(&diff, "@@ -%s +%s @@\n",
			reviewRange(beforeOffset+first.I1, beforeOffset+last.I2),
			reviewRange(afterOffset+first.J1, afterOffset+last.J2))
		for _, op := range group {
			if op.Tag == 'e' {
				writeReviewLines(&diff, ' ', a[op.I1:op.I2])
			}
			if op.Tag == 'r' || op.Tag == 'd' {
				writeReviewLines(&diff, '-', a[op.I1:op.I2])
			}
			if op.Tag == 'r' || op.Tag == 'i' {
				writeReviewLines(&diff, '+', b[op.J1:op.J2])
			}
		}
	}
	file.Diff = diff.String()
	return file
}

func reviewPath(path string) string {
	if path == "" {
		return "/dev/null"
	}
	return fmt.Sprintf("%q", path)
}

func reviewRange(start, end int) string {
	if start == end {
		return fmt.Sprintf("%d,0", start)
	}
	return fmt.Sprintf("%d,%d", start+1, end-start)
}

func writeReviewLines(output *strings.Builder, prefix byte, lines []string) {
	for _, line := range lines {
		output.WriteByte(prefix)
		output.WriteString(line)
		if !strings.HasSuffix(line, "\n") {
			output.WriteString("\n\\ No newline at end of file\n")
		}
	}
}
