package launch

import (
	"io"
	"strings"
	"testing"
	"time"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/testsupport/uisnapshot"
)

func TestUISnapshotNativeProcessCountBreathingWhileIdle(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.server.mu.Lock()
	f.server.bgTerminals = []map[string]any{
		{"threadId": "thr-main", "turnId": "finished", "itemId": "one", "processId": "one"},
		{"threadId": "thr-main", "turnId": "finished", "itemId": "two", "processId": "two"},
	}
	f.server.mu.Unlock()
	if err := f.conn.refreshProcesses(f.ctx, "thr-main"); err != nil {
		t.Fatal(err)
	}
	ev := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Status.Processes == 2 })
	now := time.Unix(1700000080, 0)
	const width = 82
	f.client.renderer.terminal.Width, f.client.renderer.terminal.Height = width, 13
	f.client.accept(ev)
	var countRows []string
	for _, phase := range []time.Duration{0, time.Second} {
		at := now.Add(phase)
		if !f.client.advancePresentation(at) {
			t.Fatal("an idle turn stopped the background process animation")
		}
		f.client.refresh(at)
		rendered, err := f.client.renderer.render(f.ctx, io.Discard, at, "0.172.0")
		if err != nil {
			t.Fatal(err)
		}
		for _, row := range rendered.Rows {
			if strings.Contains(row, "2 running processes") {
				countRows = append(countRows, row)
				break
			}
		}
	}
	if len(countRows) != 2 || countRows[0] == countRows[1] {
		t.Fatal("running process count did not change color across the breathing cycle")
	}
	uisnapshot.AssertTerminal(t, "testdata/snapshots/native-parity-process-count-breathing.txt", countRows, width)
}
