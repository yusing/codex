package launch

import (
	"slices"
	"testing"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/terminalsession"
)

func TestUISnapshotNativeLoadedSkills(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1","item":{"type":"commandExecution",
		"id":"get","command":"skills-mgr get review-pr/references/check.md","status":"completed","exitCode":0,
		"display":{"script":"skills-mgr get review-pr/references/check.md","segments":[],"stepKinds":["skill"],
		"stepSubjects":["review-pr/references/check.md"]}}}`)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1","item":{"type":"commandExecution",
		"id":"cat","command":"cat /installed/checker/SKILL.md","status":"completed","exitCode":0}}`)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1","item":{"type":"commandExecution",
		"id":"fail","command":"skills-mgr get missing","status":"failed","exitCode":1}}`)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1","item":{"type":"commandExecution",
		"id":"run","command":"skills-mgr run only-script/example.sh","status":"completed","exitCode":0}}`)
	ev := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 4
	})
	if !slices.Equal(ev.Frame.View.Info.Skills, []string{"review-pr", "checker"}) {
		t.Fatalf("loaded skills = %v", ev.Frame.View.Info.Skills)
	}
	f.assert("loaded-skills", ev, 110, 14)
	if !slices.ContainsFunc(f.client.renderer.terminal.Shell.Targets, func(target terminalsession.StatTarget) bool {
		return target.Action == terminalsession.StatSkills && target.W > 0
	}) {
		t.Fatal("loaded skills stat has no clickable terminal target")
	}
	f.send(sessionapi.NativeOperation{Details: &sessionapi.NativeDetailRequest{
		Kind: sessionapi.NativeSkillDetails, Displayed: ev.Frame.View.Main,
	}})
	details := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.Details != nil })
	f.assert("loaded-skills-details", details, 110, 14)
}
