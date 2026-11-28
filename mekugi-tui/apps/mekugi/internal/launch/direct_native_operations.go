package launch

import (
	"cmp"
	"context"
	"crypto/rand"
	"encoding/json/jsontext"
	json "encoding/json/v2"
	"errors"
	"fmt"
	"net/http"
	"regexp"
	"slices"
	"strings"
	"time"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessioncontrol"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/binding"
	activityfeed "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/feed"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/codexbinding"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/completion"
	conversation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presentation"
)

func (c *directNativeConnection) Send(ctx context.Context, target sessionapi.NativeTarget, op sessionapi.NativeOperation) (sessionapi.NativeResult, error) {
	var err error
	switch {
	case op.Composer != nil:
		err = c.handleComposer(ctx, *op.Composer)
	case op.Command != nil:
		if *op.Command == "/presets" && op.Editor != nil {
			seed := ""
			if len(op.Editor.Draft.Spans()) == 0 {
				seed = op.Editor.Draft.Text
			}
			err = c.openPresetMenu(ctx, seed)
		} else {
			err = c.handleSlashCommand(ctx, *op.Command, sessionpresentation.ComposerIntent{})
		}
	case op.Interrupt != nil:
		err = c.handleInterrupt(ctx, *op.Interrupt)
	case op.Approval != nil:
		err = c.handleApproval(ctx, *op.Approval)
	case op.Answers != nil:
		err = c.handleAnswers(ctx, *op.Answers)
	case op.Display != nil:
		err = c.handleDisplay(ctx, *op.Display)
	case op.Side != nil:
		err = c.handleCloseSide(ctx)
	case op.BTW != nil:
		err = c.handleCloseBTW(ctx, *op.BTW)
	case op.Resume != nil:
		err = c.handleResumeAction(ctx, *op.Resume)
	case op.Output != nil:
		c.handleOutputDemand(*op.Output)
	case op.DetailOutput != nil:
		c.handleDetailOutput(*op.DetailOutput)
	case op.CloseDetails != nil:
		c.mu.Lock()
		if c.details != nil && c.details.View.ID == *op.CloseDetails {
			c.details = nil
			c.publishLocked()
		}
		c.mu.Unlock()
	case op.Preferences != nil:
		err = c.saveTerminalPreferences(ctx, target, op.Preferences.Presentation)
	case op.Focused != nil:
		c.mu.Lock()
		c.focused = *op.Focused
		c.mu.Unlock()
	case op.Completion != nil:
		c.handleCompletion(ctx, *op.Completion)
	case op.Menu != nil:
		err = c.handleMenu(ctx, *op.Menu, op.Editor)
	case op.Receipt != nil:
		c.handleReceipt(*op.Receipt)
	case op.Details != nil:
		err = c.handleOpenDetails(ctx, *op.Details)
	case op.Panel != nil:
		err = c.handlePanelAction(ctx, *op.Panel)
	case op.StopProcess != nil:
		err = c.handleStopProcess(ctx, *op.StopProcess)
	case op.Reference != nil:
		c.handleReference(*op.Reference)
	case op.Notice != nil:
		c.handleNotice(*op.Notice)
	case op.SaveDraft != nil:
		err = c.saveInputHistory(ctx, *op.SaveDraft)
	}
	c.mu.Lock()
	ver := sessionpresentation.Version{Attachment: c.attachID, Revision: c.revision}
	c.mu.Unlock()
	return sessionapi.NativeResult{Version: ver}, err
}

func (c *directNativeConnection) handleComposer(ctx context.Context, in sessionapi.NativeComposerInput) error {
	text := strings.TrimSpace(in.Draft.Text)
	switch in.Intent.Operation {
	case sessionpresentation.IncreaseEffort:
		return c.stepReasoningEffort(ctx, +1)
	case sessionpresentation.DecreaseEffort:
		return c.stepReasoningEffort(ctx, -1)
	case sessionpresentation.RestoreWaiting:
		return c.editWaiting(ctx, in.Intent.Thread)
	case sessionpresentation.SteerWaiting:
		return c.steerWaiting(ctx, in.Intent.Thread)
	}
	hasAttachments := len(in.Draft.Images) > 0 || len(in.Draft.AnswerImages) > 0 || in.Draft.AnswerSelections != "" || in.Draft.UserSelections() != ""
	if text == "" && !hasAttachments {
		return nil
	}
	if strings.HasPrefix(text, "/") {
		if hasAttachments {
			return &sessionapi.NativeFailure{Status: http.StatusBadRequest, Reason: "attachments are not supported with slash commands"}
		}
		return c.handleSlashCommand(ctx, text, in.Intent)
	}
	if strings.HasPrefix(text, "!") {
		if hasAttachments {
			return &sessionapi.NativeFailure{Status: http.StatusBadRequest, Reason: "attachments are not supported with shell commands"}
		}
		cmd := strings.TrimSpace(strings.TrimPrefix(text, "!"))
		if cmd == "" {
			return nil
		}
		c.mu.Lock()
		threadID := c.mainThread
		c.mu.Unlock()
		if threadID == "" {
			return errors.New("no active thread")
		}
		method := "thread/shellCommand"
		if in.Intent.Operation == sessionpresentation.QueueComposer {
			method = "thread/queue/shellCommand"
		}
		err := c.wire.Call(ctx, method, map[string]any{
			"threadId": threadID,
			"command":  cmd,
		}, nil)
		if refusal, ok := errors.AsType[*nativeAppServerRPCError](err); ok {
			switch refusal.Code {
			case -32600, -32601, -32602:
				return &sessionapi.NativeFailure{Status: http.StatusBadRequest, Reason: refusal.Error()}
			}
		}
		return err
	}

	c.mu.Lock()
	threadID := in.Intent.Thread
	if threadID == "" {
		threadID = c.inputThread
	}
	if threadID == "" {
		threadID = c.mainThread
	}
	turnID := c.activeTurnForLocked(threadID)
	busy := turnID != ""
	c.mu.Unlock()
	if threadID == "" {
		return errors.New("no active thread")
	}
	draft := in.Draft.Trimmed()
	input := codexbinding.Input(draft)
	if in.Intent.Operation == sessionpresentation.QueueComposer && busy {
		return c.wire.Call(ctx, "thread/queue/add", map[string]any{
			"threadId":            threadID,
			"input":               input,
			"clientUserMessageId": rand.Text(),
		}, nil)
	}
	if busy {
		err := c.steerInput(ctx, directSteerTurnID{thread: threadID, turn: turnID},
			&directSteer{clientID: rand.Text(), draft: draft}, input, nil)
		if refusal, ok := errors.AsType[*nativeAppServerRPCError](err); ok {
			switch refusal.Code {
			case -32600, -32602:
				return &sessionapi.NativeFailure{Status: http.StatusBadRequest, Reason: refusal.Error()}
			}
		}
		return err
	}
	return c.startInputTurn(ctx, threadID, input)
}

// A turn may complete before its turn/start reply reaches the client.
// Keep terminal IDs only for the lifetime of each outstanding start RPC.
type directInputTurnStart struct {
	threadID  string
	completed []string
}

// Display the first input while native turn admission is still in flight.
type directFirstInput struct {
	clientID string
	turnID   string
	entry    activity.Entry
}

func (c *directNativeConnection) startInputTurn(ctx context.Context, threadID string, input []map[string]any) error {
	start := &directInputTurnStart{threadID: threadID}
	params := map[string]any{"threadId": threadID, "input": input}
	var clientID string
	c.mu.Lock()
	if _, pending := c.firstInputs[threadID]; !pending && len(c.items[threadID]) == 0 {
		content, err := json.Marshal(input)
		if err != nil {
			c.mu.Unlock()
			return err
		}
		message, spans := extractUserMessage(content)
		clientID = rand.Text()
		params["clientUserMessageId"] = clientID
		c.seq++
		c.firstInputs[threadID] = directFirstInput{
			clientID: clientID,
			entry: activity.Entry{
				Facts: activity.Facts{
					Seq: c.seq, Agent: "You", Kind: "user", Text: message, Spans: spans,
					Observed: time.Now(), Unconfirmed: true,
				},
				Revision: 1,
			},
		}
		c.publishLocked()
	}
	c.inputTurnStarts = append(c.inputTurnStarts, start)
	c.mu.Unlock()

	var reply struct {
		Turn struct {
			binding.Turn
			StartedAt *int64 `json:"startedAt"`
		} `json:"turn"`
	}
	err := c.wire.Call(ctx, "turn/start", params, &reply)

	c.mu.Lock()
	defer c.mu.Unlock()
	c.inputTurnStarts = slices.DeleteFunc(c.inputTurnStarts, func(request *directInputTurnStart) bool {
		return request == start
	})
	if first, ok := c.firstInputs[threadID]; ok && first.clientID == clientID {
		if err != nil || reply.Turn.ID == "" || slices.Contains(start.completed, reply.Turn.ID) {
			delete(c.firstInputs, threadID)
			c.publishLocked()
		} else {
			first.turnID = reply.Turn.ID
			c.firstInputs[threadID] = first
		}
	}
	if err != nil {
		return err
	}
	if threadID == c.sideThread && c.sideFirstTurn == "" {
		c.sideFirstTurn = reply.Turn.ID
	}
	if threadID != "" && (threadID == c.mainThread || threadID == c.sideThread) &&
		reply.Turn.ID != "" && reply.Turn.Status == "inProgress" &&
		c.activeTurnForLocked(threadID) == "" && !slices.Contains(start.completed, reply.Turn.ID) {
		c.setActiveInputTurnLocked(threadID, reply.Turn.ID, reply.Turn.StartedAt)
		c.publishLocked()
	}
	return nil
}

func (c *directNativeConnection) handleSlashCommand(ctx context.Context, raw string, intent sessionpresentation.ComposerIntent) error {
	trimmed := strings.TrimSpace(raw)
	verb, rest, _ := strings.Cut(trimmed, " ")
	rest = strings.TrimSpace(rest)
	c.mu.Lock()
	mainID := c.mainThread
	if (verb != "/permissions" || rest != "") && c.isPermissionMenuLocked() {
		c.clearMenuLocked()
		c.publishLocked()
	}
	c.mu.Unlock()

	switch verb {
	case "/quit":
		return c.requestQuit(ctx, mainID)
	case "/detach":
		c.finish(&sessionapi.NativeExit{Code: 0})
		return nil
	case "/compact":
		if rest != "" {
			return errors.New("usage: /compact")
		}
		c.mu.Lock()
		threadID := cmp.Or(intent.Thread, c.inputThread, mainID)
		busy := c.activeTurnForLocked(threadID) != ""
		c.mu.Unlock()
		if threadID == "" {
			return errors.New("no active thread")
		}
		mode := "continueInterrupted"
		if intent.Operation == sessionpresentation.QueueComposer && busy {
			mode = "queue"
		}
		return c.wire.Call(ctx, "thread/compact/start", map[string]string{"threadId": threadID, "mode": mode}, nil)
	case "/archive":
		if mainID == "" {
			return errors.New("no active thread")
		}
		c.mu.Lock()
		explanation := "Are you sure? This will archive the current session."
		if c.busy {
			explanation = "This will stop the current turn and archive the session."
		}
		c.openCommandConfirmationLocked(sessionpresentation.MenuArchive, "Archive this session?", explanation, []completion.Choice{
			{ID: 1, Name: "No, don't archive", Description: "Return to the current session"},
			{ID: 2, Name: "Yes, archive", Description: "Archive this session now", Danger: true},
		})
		c.mu.Unlock()
		return nil
	case "/clear":
		return c.clearThread(ctx)
	case "/resume":
		c.mu.Lock()
		active := c.activeTurnForLocked(cmp.Or(intent.Thread, c.inputThread)) != ""
		c.mu.Unlock()
		if active {
			return errors.New("'/resume' is disabled while a task is in progress")
		}
		if rest == "" {
			return c.openResumePicker(ctx)
		}
		return c.resumeByIDOrName(ctx, rest)
	case "/rewind":
		if rest != "" {
			return errors.New("Usage: /rewind")
		}
		return c.openRewind(ctx)
	case "/model":
		if rest != "" {
			return errors.New("Usage: /model")
		}
		threadID, model, _ := c.selectedModelSettings()
		if threadID == "" {
			return errors.New("no active thread")
		}
		return c.openModelMenu(ctx, threadID, model)
	case "/effort", "/reasoning":
		threadID, model, effort := c.selectedModelSettings()
		if threadID == "" {
			return errors.New("no active thread")
		}
		if rest == "" {
			return c.openEffortMenu(ctx, threadID, model, effort)
		}
		return c.wire.Call(ctx, "thread/settings/update", map[string]any{
			"threadId": threadID,
			"effort":   rest,
		}, nil)
	case "/tier":
		if mainID == "" {
			return errors.New("no active thread")
		}
		if rest == "" {
			return c.openTierMenu(ctx, mainID)
		}
		return c.applyServiceTier(ctx, mainID, rest)
	case "/permissions":
		return c.handlePermissions(ctx, mainID, rest)
	case "/presets":
		return c.openPresetMenu(ctx, "")
	case "/oneoff":
		return c.startSide(ctx, rest)
	case "/btw":
		return c.startBTW(ctx, rest)
	case "/title":
		if mainID == "" {
			return errors.New("no active thread")
		}
		if rest == "" {
			c.mu.Lock()
			c.setFeedbackLocked("Usage: /title <new title>", true)
			c.publishLocked()
			c.mu.Unlock()
			return nil
		}
		if err := c.wire.Call(ctx, "thread/name/set", map[string]any{
			"threadId": mainID,
			"name":     rest,
		}, nil); err != nil {
			return err
		}
		c.mu.Lock()
		c.title = rest
		c.setFeedbackLocked("Title updated", false)
		c.publishLocked()
		c.mu.Unlock()
		return nil
	case "/status":
		return c.openStatusPanel(ctx)
	case "/session":
		return c.openSessionPanel(ctx)
	case "/ps":
		return c.handleOpenDetails(ctx, sessionapi.NativeDetailRequest{Kind: sessionapi.NativeProcessDetails})
	case "/skills":
		c.openSkillsMenu()
		return nil
	case "/fork":
		return c.forkAndSwitchThread(ctx, rest)
	case "/lock":
		if rest != "" {
			return errors.New("Usage: /lock")
		}
		c.mu.Lock()
		c.interruptLocked = true
		c.emitPresentationEffectLocked("Keyboard interrupt locked (/unlock to re-enable)")
		c.mu.Unlock()
		return nil
	case "/unlock":
		if rest != "" {
			return errors.New("Usage: /unlock")
		}
		c.mu.Lock()
		c.interruptLocked = false
		c.emitPresentationEffectLocked("Keyboard interrupt unlocked")
		c.mu.Unlock()
		return nil
	case "/live":
		if rest != "" {
			return errors.New("Usage: /live")
		}
		c.mu.Lock()
		c.liveHidden = !c.liveHidden
		msg := "Live edits shown"
		if c.liveHidden {
			msg = "Live edits hidden"
		}
		c.emitPresentationEffectLocked(msg)
		c.mu.Unlock()
		return nil
	case "/predictions":
		if rest != "" {
			return errors.New("Usage: /predictions")
		}
		c.mu.Lock()
		c.predictionsDisabled = !c.predictionsDisabled
		msg := "Predictions enabled"
		if c.predictionsDisabled {
			c.resetPredictionLocked(c.prediction.turn)
			msg = "Predictions disabled"
		}
		c.emitPresentationEffectLocked(msg)
		c.mu.Unlock()
		return nil
	case "/copy":
		c.openCopyMenu()
		return nil
	default:
		c.mu.Lock()
		c.setFeedbackLocked("Unknown command: "+verb, true)
		c.publishLocked()
		c.mu.Unlock()
		return nil
	}
}

func (c *directNativeConnection) clearThread(ctx context.Context) error {
	params := c.newThreadParams()
	c.mu.Lock()
	if c.mainThread == "" || c.busy || len(c.mainQueue.pending) != 0 {
		c.mu.Unlock()
		return errors.New("wait for the session to be ready and idle before clearing")
	}
	previous := c.mainThread
	params["cwd"] = c.workspace
	params["sessionStartSource"] = "clear"
	params["serviceTier"] = directServiceTierValue(c.tier)
	if c.model != "" {
		params["model"] = c.model
	}
	if c.modelProvider != "" {
		params["modelProvider"] = c.modelProvider
	}
	config, _ := params["config"].(map[string]any)
	if c.effort != "" {
		if config == nil {
			config = make(map[string]any)
			params["config"] = config
		}
		config["model_reasoning_effort"] = c.effort
	} else {
		delete(config, "model_reasoning_effort")
	}
	c.mu.Unlock()
	if err := c.startFreshThread(ctx, params); err != nil {
		return err
	}
	if err := c.wire.Call(ctx, "thread/unsubscribe", map[string]any{"threadId": previous}, nil); err != nil {
		c.mu.Lock()
		c.setFeedbackLocked("New thread started; could not release the previous thread: "+err.Error(), true)
		c.publishLocked()
		c.mu.Unlock()
	}
	return nil
}

func (c *directNativeConnection) stepReasoningEffort(ctx context.Context, delta int) error {
	threadID, currentModel, currentEffort := c.selectedModelSettings()
	if threadID == "" {
		return nil
	}
	models, err := c.listWireModels(ctx, threadID)
	if err != nil {
		return err
	}
	model, _ := findWireModel(models, currentModel)
	var levels []string
	for _, effort := range model.SupportedReasoningEfforts {
		if effort.ReasoningEffort != "" {
			levels = append(levels, effort.ReasoningEffort)
		}
	}
	if len(levels) == 0 {
		return errors.New("No advertised reasoning effort choices")
	}
	idx := slices.Index(levels, cmp.Or(currentEffort, model.DefaultReasoningEffort))
	if idx < 0 {
		idx = 0
	}
	next := min(max(idx+delta, 0), len(levels)-1)
	return c.wire.Call(ctx, "thread/settings/update", map[string]any{
		"threadId": threadID,
		"effort":   levels[next],
	}, nil)
}

func (c *directNativeConnection) startSide(ctx context.Context, promptText string) error {
	if promptText == "" {
		return errors.New("provide a task for /oneoff")
	}
	c.mu.Lock()
	mainID := c.mainThread
	if c.sideThread != "" {
		c.openCommandConfirmationLocked(sessionpresentation.MenuSideReplace, "Replace One-off?",
			"Replace closes only One-off, interrupts its running work and creates a fresh snapshot of Main.", []completion.Choice{
				{ID: 1, Name: "Cancel"},
				{ID: 2, Name: "Replace", Danger: true},
			})
		c.sideReplaceTask = promptText
		c.mu.Unlock()
		return nil
	}
	c.mu.Unlock()
	if mainID == "" {
		return errors.New("no active thread")
	}
	var resp directThreadStartResponse
	params := map[string]any{
		"threadId":     mainID,
		"threadSource": "mekugi_side",
		"excludeTurns": true,
	}
	if err := c.wire.Call(ctx, "thread/fork", params, &resp); err != nil {
		return fmt.Errorf("fork side thread: %w", err)
	}
	sideID := resp.Thread.ID
	c.mu.Lock()
	c.sideThread = sideID
	c.sideFirstTurn = ""
	c.sideModel, c.sideEffort = resp.Model, resp.ReasoningEffort
	c.inputThread = sideID
	c.displayedThread = sideID
	c.threadCallers[sideID] = sessionpresentation.SideCaller
	c.applyThreadUsageLocked(sideID, resp.Thread.TokenUsage)
	c.observeThreadSkillsLocked(sideID, resp.Thread.Extra.Skills)
	c.publishLocked()
	c.mu.Unlock()
	return c.startInputTurn(ctx, sideID, appserver.Input(promptText))
}

func (c *directNativeConnection) handleCloseSide(ctx context.Context) error {
	c.mu.Lock()
	sideID := c.sideThread
	c.mu.Unlock()
	if sideID == "" {
		return nil
	}
	if err := c.wire.Call(ctx, "thread/archive", map[string]any{"threadId": sideID}, nil); err != nil {
		return err
	}
	c.mu.Lock()
	delete(c.firstInputs, sideID)
	c.sideThread = ""
	c.sideFirstTurn = ""
	c.sideModel, c.sideEffort = "", ""
	c.sideActiveTurn = ""
	c.sideTurnStatus = sessionpresentation.Label{}
	c.sideQueue = directQueueState{}
	if c.settingsMenuThread == sideID || c.menu != nil && c.menu.Kind == sessionpresentation.MenuSideReplace {
		c.clearMenuLocked()
	}
	c.busy = c.activeTurn != ""
	c.inputThread = c.mainThread
	c.displayedThread = c.mainThread
	c.publishLocked()
	c.mu.Unlock()
	return nil
}

func (c *directNativeConnection) startBTW(ctx context.Context, question string) error {
	if question == "" {
		return errors.New("provide a question for /btw")
	}
	c.mu.Lock()
	mainID := c.mainThread
	btwID := c.btwThread
	btwMain := c.btwMainThread
	btwRunning := c.btwRunning
	btwStatus := c.btw.Status
	c.mu.Unlock()
	if mainID == "" {
		return errors.New("no active thread")
	}
	if btwID != "" && btwMain != mainID {
		if err := c.archiveBTW(ctx, btwID); err != nil {
			return err
		}
		btwID = ""
		btwRunning = false
		btwStatus = ""
	}
	if btwRunning {
		return &sessionapi.NativeFailure{Status: http.StatusConflict, Reason: "wait for the current /btw answer to finish"}
	}
	if btwStatus == conversation.SideCompactionRequired {
		return errors.New(btwStatus)
	}
	if btwID == "" {
		var resp directThreadStartResponse
		if err := c.wire.Call(ctx, "thread/fork", map[string]any{
			"threadId":       mainID,
			"threadSource":   "mekugi_btw",
			"excludeTurns":   true,
			"sandbox":        "read-only",
			"approvalPolicy": "never",
		}, &resp); err != nil {
			return fmt.Errorf("fork side answer: %w", err)
		}
		btwID = resp.Thread.ID
	}
	c.mu.Lock()
	c.btwThread = btwID
	c.btwMainThread = mainID
	c.btw.Begin(question)
	exchange := c.btw.ID
	c.btwRunning = true
	c.publishLocked()
	c.mu.Unlock()
	err := c.wire.Call(ctx, "turn/start", map[string]any{
		"threadId":     btwID,
		"input":        appserver.Input(question),
		"environments": []any{},
	}, nil)
	if err != nil {
		c.mu.Lock()
		if c.btwThread == btwID && c.btw.ID == exchange {
			c.btwRunning = false
			c.btw.Status, c.btw.Alert = err.Error(), true
			c.publishLocked()
		}
		c.mu.Unlock()
	}
	return err
}

func (c *directNativeConnection) handleCloseBTW(ctx context.Context, intent sessionpresentation.CloseSideIntent) error {
	c.mu.Lock()
	btwID := c.btwThread
	current := btwID != "" && intent.Key == btwID && intent.Exchange == c.btw.ID
	c.mu.Unlock()
	if !current {
		return sessionpresentation.ErrStaleIntent
	}
	return c.archiveBTW(ctx, btwID)
}

func (c *directNativeConnection) archiveBTW(ctx context.Context, btwID string) error {
	if err := c.wire.Call(ctx, "thread/archive", map[string]any{"threadId": btwID}, nil); err != nil {
		return err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.btwThread != btwID {
		return nil
	}
	c.btwThread = ""
	c.btwMainThread = ""
	c.btw = conversation.SideAnswer{}
	c.btwCompletedAnswer = ""
	c.btwRunning = false
	c.publishLocked()
	return nil
}

func (c *directNativeConnection) handleInterrupt(ctx context.Context, intent sessionpresentation.InterruptIntent) error {
	c.mu.Lock()
	threadID := intent.Target.Thread
	if threadID == "" {
		threadID = c.inputThread
	}
	if threadID == "" {
		threadID = c.mainThread
	}
	activeTurn := c.activeTurnForLocked(threadID)
	turnID := cmp.Or(intent.Target.Turn, activeTurn)
	key := directSteerTurnID{thread: threadID, turn: turnID}
	var steers *directSteerTurn
	if intent.Mode == sessioncontrol.RestoreWaiting && turnID != "" && turnID == activeTurn &&
		c.queueForThreadLocked(threadID) != nil {
		steers = c.steerTurnLocked(key)
	}
	c.mu.Unlock()
	if threadID == "" {
		return nil
	}
	params := map[string]any{"threadId": threadID}
	if turnID != "" {
		params["turnId"] = turnID
	}
	if steers == nil {
		return c.wire.Call(ctx, "turn/interrupt", params, nil)
	}
	return c.interruptAndRestore(ctx, key, steers, params)
}

func (c *directNativeConnection) handleApproval(ctx context.Context, intent sessionpresentation.ApprovalIntent) error {
	c.mu.Lock()
	req := c.requests[intent.Request]
	if req == nil || req.approval == nil {
		c.mu.Unlock()
		return sessionpresentation.ErrStaleIntent
	}
	decision := "decline"
	if intent.Choice >= 0 && intent.Choice < len(req.approval.Choices) {
		choice := req.approval.Choices[intent.Choice]
		switch {
		case choice.Approve && choice.Session:
			decision = "acceptForSession"
		case choice.Approve:
			decision = "accept"
		case choice.Deny:
			decision = "decline"
		}
	}
	delete(c.requests, intent.Request)
	c.requestOrder = slices.DeleteFunc(c.requestOrder, func(id string) bool { return id == intent.Request })
	c.publishLocked()
	c.mu.Unlock()

	if req.method == "item/permissions/requestApproval" {
		var payload struct {
			Permissions jsontext.Value `json:"permissions"`
		}
		_ = json.Unmarshal(req.rawParams, &payload)
		scope := "turn"
		if decision == "acceptForSession" {
			scope = "session"
		}
		if decision == "decline" || len(payload.Permissions) == 0 {
			return c.wire.Respond(ctx, req.id, map[string]any{
				"permissions": map[string]any{},
				"scope":       "turn",
			})
		}
		return c.wire.Respond(ctx, req.id, map[string]any{
			"permissions": payload.Permissions,
			"scope":       scope,
		})
	}
	if req.shellGuard {
		result := map[string]any{"decision": decision}
		if intent.Reason != "" {
			result["reason"] = intent.Reason
		}
		return c.wire.Respond(ctx, req.id, result)
	}
	result := map[string]any{"decision": decision}
	if intent.Reason != "" {
		result["reason"] = intent.Reason
	}
	return c.wire.Respond(ctx, req.id, result)
}

func (c *directNativeConnection) handleAnswers(ctx context.Context, in sessionapi.NativeAnswers) error {
	c.mu.Lock()
	req := c.requests[in.Call]
	if req == nil || req.question == nil {
		c.mu.Unlock()
		return sessionpresentation.ErrStaleIntent
	}
	if !req.question.Sync {
		return c.answerAsyncQuestions(ctx, in, req)
	}
	delete(c.requests, in.Call)
	c.requestOrder = slices.DeleteFunc(c.requestOrder, func(id string) bool { return id == in.Call })
	c.publishLocked()
	c.mu.Unlock()

	if req.secret {
		secret := ""
		declined := in.Incomplete || len(in.Answers) == 0
		if len(in.Answers) > 0 {
			if in.Answers[0].Skip {
				declined = true
			}
			secret = in.Answers[0].Secret
			if secret == "" {
				secret = in.Answers[0].Draft.Text
			}
		}
		if declined || secret == "" {
			return c.wire.Respond(ctx, req.id, map[string]any{"decision": "decline"})
		}
		return c.wire.Respond(ctx, req.id, map[string]any{"decision": "provide", "secret": secret})
	}
	answers := make([]binding.QuestionAnswer, 0, len(in.Answers))
	for _, ans := range in.Answers {
		other := ans.Draft.Text
		if ans.Secret != "" {
			other = ans.Secret
		}
		answers = append(answers, binding.QuestionAnswer{
			ID:      ans.Question,
			Choice:  ans.Choice,
			Note:    ans.Note,
			Other:   other,
			Skipped: ans.Skip,
		})
	}
	return c.wire.Respond(ctx, req.id, binding.EncodeQuestionResponse(answers))
}

func (c *directNativeConnection) handleDisplay(ctx context.Context, intent sessionpresentation.DisplayIntent) error {
	if intent.Thread.ID == "" {
		return nil
	}
	c.mu.Lock()
	c.displayedThread = intent.Thread.ID
	if intent.Thread.Caller != "" {
		c.threadCallers[intent.Thread.ID] = intent.Thread.Caller
	}
	if intent.Thread.ID == c.mainThread || intent.Thread.ID == c.sideThread {
		c.inputThread = intent.Thread.ID
	}
	c.publishLocked()
	c.mu.Unlock()
	// Drive Herdr/Agents/Saved Changes panels from the attached frontend's selection.
	go func(threadID string) {
		c.reloadThreadTranscript(c.ctx, threadID)
		c.requestChanges(threadID)
	}(intent.Thread.ID)
	return nil
}

const resumeHyphenatedUUID = `[[:xdigit:]]{8}(?:-[[:xdigit:]]{4}){3}-[[:xdigit:]]{12}`

var resumeUUID = regexp.MustCompile(`^(?:[[:xdigit:]]{32}|` + resumeHyphenatedUUID +
	`|\{` + resumeHyphenatedUUID + `\}|urn:uuid:` + resumeHyphenatedUUID + `)$`)

func (c *directNativeConnection) resumeByIDOrName(ctx context.Context, target string) error {
	c.mu.Lock()
	current := c.mainThread
	c.mu.Unlock()
	if resumeUUID.MatchString(target) {
		id := strings.Trim(strings.TrimPrefix(target, "urn:uuid:"), "{}")
		if strings.EqualFold(strings.ReplaceAll(id, "-", ""), strings.ReplaceAll(current, "-", "")) {
			return nil
		}
		return c.resumeThread(ctx, target)
	}

	// Native list search narrows candidates; only an exact, unique display label
	// is accepted, as in the stock terminal's named-session lookup.
	params := map[string]any{
		"limit": 100, "modelProviders": []string{}, "archived": false,
		"useStateDbOnly": true, "sortKey": "created_at", "searchTerm": target,
	}
	var match string
	for {
		var page directResumePage
		if err := c.wire.Call(ctx, "thread/list", params, &page); err != nil {
			return fmt.Errorf("find saved session: %w", err)
		}
		for _, th := range page.Data {
			if th.ID == "" || th.ThreadSource == "mekugi_side" || resumeThreadLabel(th) != target {
				continue
			}
			if match != "" && match != th.ID {
				return fmt.Errorf("multiple sessions match %q; use a session UUID to disambiguate", target)
			}
			match = th.ID
		}
		if page.NextCursor == "" {
			break
		}
		params["cursor"] = page.NextCursor
	}
	if match == "" {
		return fmt.Errorf("no saved chat found matching %q", target)
	}
	if match == current {
		return nil
	}
	return c.resumeThread(ctx, match)
}

func (c *directNativeConnection) handleResumeAction(ctx context.Context, action sessionpresentation.ResumeAction) error {
	c.mu.Lock()
	view := c.resumeView
	if view == nil || action.Picker != view.ID {
		c.mu.Unlock()
		return nil
	}
	active := c.activeTurnForLocked(c.inputThread) != ""
	current := c.mainThread
	c.mu.Unlock()

	next := *view
	switch action.Kind {
	case sessionpresentation.ResumeChoose:
		if !slices.ContainsFunc(view.Rows, func(row sessionpresentation.ResumeRow) bool { return row.ID == action.Thread }) {
			return nil
		}
		if active {
			return c.reportResumeError(view, errors.New("'/resume' is disabled while a task is in progress"), false)
		}
		if view.Archived {
			if err := c.wire.Call(ctx, "thread/unarchive", map[string]string{"threadId": action.Thread}, nil); err != nil {
				return c.reportResumeError(view, fmt.Errorf("restore archived session: %w", err), false)
			}
			// Restoration succeeded even if attaching to the session fails.
			next.Rows = slices.DeleteFunc(slices.Clone(view.Rows), func(row sessionpresentation.ResumeRow) bool {
				return row.ID == action.Thread
			})
			c.mu.Lock()
			if c.resumeView != view {
				c.mu.Unlock()
				return nil
			}
			c.resumeView = &next
			c.publishLocked()
			c.mu.Unlock()
			view = &next
		}
		if action.Thread != current {
			if err := c.resumeThread(ctx, action.Thread); err != nil {
				return c.reportResumeError(view, err, false)
			}
			return nil
		}
	case sessionpresentation.ResumePage:
		if action.After == "" || action.After != view.PageAfter {
			return nil
		}
		return c.loadResumeList(ctx, view, next)
	case sessionpresentation.ResumeToggleWorkspace, sessionpresentation.ResumeToggleArchived:
		if action.Kind == sessionpresentation.ResumeToggleArchived {
			next.Archived = !view.Archived
		} else {
			next.All = !view.All
		}
		next.Rows, next.PageAfter = nil, ""
		return c.loadResumeList(ctx, view, next)
	case sessionpresentation.ResumeDismiss:
		if view.Startup {
			if err := c.startFreshThread(ctx, c.newThreadParams()); err != nil {
				return c.reportResumeError(view, err, false)
			}
			return nil
		}
	case sessionpresentation.ResumeCancel:
		if view.Startup {
			c.finish(&sessionapi.NativeExit{Code: 0})
		}
	default:
		return nil
	}
	c.mu.Lock()
	if c.resumeView == view {
		c.resumeView = nil
		c.publishLocked()
	}
	c.mu.Unlock()
	return nil
}

func (c *directNativeConnection) handleOutputDemand(demand sessionapi.NativeOutputDemand) {
	c.mu.Lock()
	defer c.mu.Unlock()
	results := make([]activity.OutputResult, 0, len(demand.Demands))
	threadID := c.mainThread
	if demand.Agent && c.displayedThread != "" {
		threadID = c.displayedThread
	}
	for _, d := range demand.Demands {
		rec := c.findRecordBySeqLocked(threadID, d.Source)
		if rec == nil {
			continue
		}
		view := activityfeed.OutputView{Lines: directOutputLines(rec.output), Done: rec.entry.OutputComplete}
		if rec.itemType == "commandExecution" && len(rec.entry.Values) != 0 {
			value := rec.entry.Values[len(rec.entry.Values)-1]
			view.Exited, view.Exit = value.Exited, value.ExitCode
		}
		results = append(results, activity.OutputResult{Demand: d, View: view})
	}
	if demand.Agent {
		c.agentOutput = results
	} else {
		c.mainOutput = results
	}
	c.publishLocked()
}
