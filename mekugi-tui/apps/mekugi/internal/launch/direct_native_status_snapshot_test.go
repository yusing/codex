package launch

import (
	"testing"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/composer"
)

func TestUISnapshotNativeComposerTurnStatus(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("turn/started", `{"threadId":"thr-main","turn":{"id":"turn-1","startedAt":1700000080}}`)
	f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Interrupt.Turn == "turn-1" && ev.Frame.View.Status.Session.Running
	})
	f.send(sessionapi.NativeOperation{Composer: &sessionapi.NativeComposerInput{
		Intent: sessionpresentation.ComposerIntent{Thread: "thr-main", Operation: sessionpresentation.SubmitComposer},
		Draft:  composer.Draft{Text: "Check retained inputs."},
	}})
	steering := f.await(func(ev sessionapi.NativeEvent) bool { return len(ev.Frame.View.Pending.Waiting) == 1 })
	f.assert("steering-status", steering, 88, 13)

	f.notify("turn/completed", `{"threadId":"thr-main","turn":{"id":"turn-1","status":"interrupted","durationMs":600}}`)
	interrupted := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Status.Session.Text == "Interrupted"
	})
	f.assert("interrupted-status", interrupted, 88, 13)

	f.notify("turn/started", `{"threadId":"thr-main","turn":{"id":"turn-2","startedAt":1700000080}}`)
	f.notify("turn/completed", `{"threadId":"thr-main","turn":{"id":"turn-1","status":"interrupted"}}`)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-2",
		"item":{"type":"userMessage","id":"second","content":[{"type":"text","text":"Finish the check."}]}}`)
	working := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 1
	})
	if !working.Frame.View.Status.Session.Running || working.Frame.View.Interrupt.Turn != "turn-2" {
		t.Fatal("old completion displaced the active turn")
	}
	f.notify("turn/completed", `{"threadId":"thr-main","turn":{"id":"turn-2","status":"completed","durationMs":10000}}`)
	completed := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Status.Session.Text == "Completed" })
	f.assert("completed-status", completed, 88, 13)
}
