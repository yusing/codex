package terminalsession

import (
	"strings"
	"testing"
	"time"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffrender"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/feed"
	activitypresentation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	activityui "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/terminalview"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/composer"
	conversationmodel "github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/testsupport/uisnapshot"
)

func TestMainFrameReturnsOutputDemands(t *testing.T) {
	var main Main
	var prompts Prompts
	thread := sessionpresentation.Thread{ID: "main", Caller: "/root"}
	main.Update(sessionpresentation.View{Main: thread, Displayed: thread})
	view := activityui.NewView()
	view.Expansion = 2
	view.Update(&activitypresentation.Publication{Entries: []activitypresentation.Entry{{
		Seq: 7, Agent: "Main", Kind: "tool", Text: "command", Outputs: map[int]int{1: 3},
		Values: []activitypresentation.Value{{Kind: "op", Verb: "Run", Label: "printf retained", Source: 7, Output: 1, Exited: true, Tail: []string{"tail"}, TailOmitted: 1}},
	}}}, activityui.Display{Caller: "/root"})
	options := MainOptions{Now: time.Unix(100, 0), Focused: true, Version: "dev"}
	frame := main.Render(view, &view.Painter, &prompts, options, 100, 24, 0)
	if len(frame.Demands) == 0 {
		t.Fatal("Main did not return required output demands")
	}
	results := make([]activitypresentation.OutputResult, len(frame.Demands))
	for i, demand := range frame.Demands {
		if demand.Source != 7 || demand.Output != 1 || demand.Version != 3 {
			t.Fatalf("output identity = %+v", demand)
		}
		results[i] = activitypresentation.OutputResult{Demand: demand, View: feed.OutputView{Lines: []string{"retained first line", "tail"}}}
	}
	view.Hydrate(results)
	frame = main.Render(view, &view.Painter, &prompts, options, 100, 24, 0)
	if len(frame.Demands) != 0 || !strings.Contains(strings.Join(frame.Rows, "\n"), "retained first line") {
		t.Fatalf("Main did not consume detached hydration: %+v\n%s", frame.Demands, strings.Join(frame.Rows, "\n"))
	}
}

func TestUISnapshotNativeWelcome(t *testing.T) {
	for _, tc := range []struct {
		name          string
		width, height int
		draft         string
		version       string
	}{
		{"launch", 80, 24, "", "dev"},
		{"first-draft", 80, 24, "Explain this repository.", "dev"},
		{"compact", 36, 8, "Explain this repository.", "dev"},
		{"full-composer", 36, 6, "first\nsecond\nthird\nfourth\nfifth", "dev"},
		{"release", 80, 24, "", "v1.2.3"},
		{"release-compact", 36, 8, "Explain this repository.", "v1.2.3"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var main Main
			var prompts Prompts
			thread := sessionpresentation.Thread{ID: "main", Caller: "/root"}
			main.Update(sessionpresentation.View{
				Main: thread, Displayed: thread,
				Info:  sessionpresentation.MainInfo{Workspace: "/workspace", BackendVersion: "0.158.0"},
				Draft: conversationmodel.DraftView{Draft: composer.Draft{Text: tc.draft}},
			})
			transcript := activityui.NewView()
			transcript.Painter.Theme = diffrender.DarkTheme
			options := MainOptions{Now: time.Unix(100, 0), Focused: true, Version: tc.version}
			frame := main.Render(transcript, &transcript.Painter, &prompts, options, tc.width, tc.height, 0)
			uisnapshot.Assert(t, "testdata/snapshots/native-welcome-"+tc.name+".txt", strings.Join(frame.Rows, "\n"))
		})
	}
}

func TestNativeWelcomeDoesNotReplaceHistoryOrLiveDock(t *testing.T) {
	for _, tc := range []struct {
		name  string
		live  livePlacement
		shown bool
	}{
		{"transcript", liveOff, false},
		{"live-dock", liveBeside, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var main Main
			var prompts Prompts
			thread := sessionpresentation.Thread{ID: "main", Caller: "/root"}
			main.Update(sessionpresentation.View{Main: thread, Displayed: thread})
			transcript := activityui.NewView()
			if !tc.shown {
				transcript.Update(&activitypresentation.Publication{Entries: []activitypresentation.Entry{{
					Seq: 1, Agent: "You", Kind: "text", Text: "Submitted prompt",
				}}}, activityui.Display{Caller: "/root"})
			}
			options := MainOptions{Now: time.Unix(100, 0), Focused: true, Version: "dev"}
			frame := main.Render(transcript, &transcript.Painter, &prompts, options, 80, 24, tc.live)
			rows := strings.Join(frame.Rows, "\n")
			if strings.Contains(rows, "Mekugi dev") ||
				!tc.shown && !strings.Contains(rows, "Submitted prompt") ||
				(frame.Dock.H > 0) != tc.shown {
				t.Fatalf("welcome displaced active content (dock: %d):\n%s", frame.Dock.H, rows)
			}
		})
	}
}

func TestLivePlacementRegions(t *testing.T) {
	// Single view with room splits horizontally beside the transcript.
	feed, live := liveBeside.regions(100, 24)
	if feed != (Rect{W: 50, H: 24}) || live != (Rect{X: 51, W: 49, H: 24}) {
		t.Fatalf("liveBeside(100,24) = feed %+v live %+v", feed, live)
	}
	// Diff open with vertical room stacks Live below the transcript.
	feed, live = liveBelow.regions(80, 30)
	if feed != (Rect{W: 80, H: 14}) || live != (Rect{Y: 14, W: 80, H: 16}) {
		t.Fatalf("liveBelow(80,30) = feed %+v live %+v", feed, live)
	}
	// Compact views fall back to replacing the transcript.
	feed, live = liveBeside.regions(60, 24)
	if feed.H != 0 || live != (Rect{W: 60, H: 24}) {
		t.Fatalf("compact liveBeside = feed %+v live %+v", feed, live)
	}
	feed, live = liveBelow.regions(80, 18)
	if feed.H != 0 || live != (Rect{W: 80, H: 18}) {
		t.Fatalf("compact liveBelow = feed %+v live %+v", feed, live)
	}
}

func TestUISnapshotPreviewWelcome(t *testing.T) {
	saved := previewWelcome
	previewWelcome = "## Preview the terminal UI\n\n" +
		"The preview runs locally and needs no credentials. Its workspace and " +
		"session data are discarded when it exits.\n\n" +
		"- Submit `demo` to start. Use **Ctrl+B, 2** to open Saved Diff.\n" +
		"- Use `/quit` to exit."
	t.Cleanup(func() { previewWelcome = saved })

	var main Main
	var prompts Prompts
	thread := sessionpresentation.Thread{ID: "main", Caller: "/root"}
	main.Update(sessionpresentation.View{Main: thread, Displayed: thread})
	transcript := activityui.NewView()
	transcript.Painter.Theme = diffrender.DarkTheme
	options := MainOptions{Now: time.Unix(100, 0), Focused: true, Version: "dev"}
	frame := main.Render(transcript, &transcript.Painter, &prompts, options, 96, 14, 0)
	uisnapshot.Assert(t, "testdata/snapshots/preview-welcome.txt", strings.Join(frame.Rows, "\n"))
}
