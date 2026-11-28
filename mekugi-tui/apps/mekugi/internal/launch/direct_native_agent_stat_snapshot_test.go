package launch

import (
	"encoding/json/jsontext"
	"testing"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffmodel"
)

func TestUISnapshotNativeAgentDiffStats(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.server.mu.Lock()
	f.server.overrides["thread/changes/read"] = func(jsontext.Value) any {
		return jsontext.Value(`{"threadId":"thr-main","workspace":"/tmp/ws",
			"changes":[
				{"changeId":"amber1","sequence":1,"threadId":"thr-main","authorThreadId":"thr-main"},
				{"changeId":"amber2","sequence":2,"threadId":"thr-worker","authorThreadId":"thr-worker"}
			],"files":[
				{"path":"main.go","absolutePath":"/tmp/ws/main.go","scope":"workspace","changeIds":["amber1"],
				 "status":"M","unifiedDiff":"@@ -1 +1,2 @@\n-old\n+new\n+more\n","additions":2,"deletions":1},
				{"path":"worker.go","absolutePath":"/tmp/ws/worker.go","scope":"workspace","changeIds":["amber2"],
				 "status":"D","unifiedDiff":"@@ -1,2 +0,0 @@\n-first\n-second\n","deletions":2}
			]}`)
	}
	f.server.mu.Unlock()
	f.conn.refreshSubtree(f.ctx, "thr-main")
	f.conn.requestChanges("thr-main")
	ev := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Family != nil && ev.Frame.Family.Agents != nil &&
			ev.Frame.Family.Agents.LineCounts["/root/worker"] == (diffmodel.Counts{Removed: 2})
	})
	if counts := ev.Frame.Family.Agents.NetCounts; counts == nil || *counts != (diffmodel.Counts{Added: 2, Removed: 3}) {
		t.Fatalf("family net counts = %v", counts)
	}
	f.client.renderer.terminal.Shell.Focus = 3
	f.assert("agent-diff-stats", ev, 110, 16)
}
