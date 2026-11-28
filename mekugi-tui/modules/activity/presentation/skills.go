package presentation

import (
	"path/filepath"
	"slices"
	"strings"

	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/commandanalysis/shellsyntax"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/commandargs"
	"mvdan.cc/sh/v3/syntax"
)

// Source: internal/router/live_activity_skills.go:12:133@[ebde9ad5].
type SkillSet []string

func (s SkillSet) With(names ...string) SkillSet {
	for _, name := range names {
		if !slices.Contains(s, name) {
			s = append(s, name)
		}
	}
	return s
}

// SkillReadNames identifies direct skill reads in a successful command. A
// compound command's exit status need not confirm every individual read.
func SkillReadNames(source string) []string {
	program, err := syntax.NewParser().Parse(strings.NewReader(source), "")
	if err != nil || len(program.Stmts) != 1 {
		return nil
	}
	stmt := program.Stmts[0]
	if script, ok := shellsyntax.ShellScript(stmt); ok {
		return SkillReadNames(script)
	}
	argv, ok := shellsyntax.PatternCall("", stmt)
	if !ok || len(argv) < 2 {
		return nil
	}
	var paths []string
	switch argv[0] {
	case "skills-mgr":
		if argv[1] == "get" && len(argv) >= 3 {
			name, _, _ := strings.Cut(argv[2], "/")
			if name != "" && !strings.HasPrefix(name, "-") {
				return []string{name}
			}
		}
	case "cat":
		paths = argv[1:]
	case "mcat":
		specs, _, err := commandargs.ParseReadBundleSource(argv[1:])
		if err == nil {
			for _, spec := range specs {
				paths = append(paths, spec.Path)
			}
		}
	}
	var names SkillSet
	for _, path := range paths {
		if dir := filepath.Dir(path); filepath.Base(path) == "SKILL.md" && dir != "." {
			names = names.With(filepath.Base(dir))
		}
	}
	return names
}
