package launch

import (
	"testing"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
)

func TestUISnapshotNativeInlineEditStats(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1","item":{
		"type":"fileChange","id":"patch","status":"completed","changes":[
			{"kind":{"type":"add"},"path":"/tmp/ws/new.go","diff":"first\n\nthird"},
			{"kind":{"type":"delete"},"path":"/tmp/ws/old.go","diff":"first\nsecond\n"},
			{"kind":{"type":"update","move_path":"/tmp/ws/next.go"},"path":"/tmp/ws/parser.go",
			 "diff":"@@ -1,2 +1,3 @@\n context\n-old\n+new\n+more\n"}
		]}}`)
	ev := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 1 &&
			len(ev.Frame.Main.Entries[0].Values) == 3
	})
	f.assert("inline-edit-stats", ev, 112, 14)
}
