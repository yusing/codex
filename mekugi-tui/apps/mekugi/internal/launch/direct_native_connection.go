package launch

import (
	"cmp"
	"context"
	"encoding/json/jsontext"
	"encoding/json/v2"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	launchoptions "github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/launch/options"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessioncontrol"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/binding"
	"github.com/mekugi-harness/mekugi/mekugi-tui/contracts/presentation"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	changes "github.com/mekugi-harness/mekugi/mekugi-tui/modules/changehistory/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/completion"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/composer"
	conversation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presets"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/prompt"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/livepreview/preview"
	usagepresentation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/usage/presentation"
)

// nativeSessionConnection abstracts the frame stream and semantic operation
// transport consumed by nativeClient.
type nativeSessionConnection interface {
	Next() (sessionapi.NativeEvent, error)
	Send(ctx context.Context, target sessionapi.NativeTarget, operation sessionapi.NativeOperation) (sessionapi.NativeResult, error)
	requestPrediction(thread, turn string, revision uint64)
	Attach(ctx context.Context, thread string) (nativeSessionConnection, error)
	Close() error
}

type directPendingServerRequest struct {
	id         jsontext.Value
	method     string
	threadID   string
	turnID     string
	itemID     string
	rawParams  jsontext.Value
	approval   *prompt.ApprovalView
	question   *prompt.CallView
	secret     bool
	shellGuard bool
}

// A fork retains item IDs from its source, but owns separate transcript rows.
type directItemKey struct{ thread, item string }

type directItemRecord struct {
	id               string
	threadID         string
	turnID           string
	itemType         string
	seq              uint64
	entry            activity.Entry
	output           string
	reasoningSummary []string
}

type directThreadUsage struct {
	sessionapi.NativeUsage
	ContextTokens         uint64
	ContextWindow         uint64
	EstimatedCostUSD      float64
	CostKnown             bool
	CostPartial           bool
	OutputTokensPerSecond *float64
	WorkDurationMs        int64
	WorkKnown             bool
	WorkSince             time.Time
	LastResponse          time.Time
	LastCompleted         time.Time
	Roundtrips            uint64
	RoundtripsPartial     bool
	Turns                 uint64
	TokensKnown           bool
	UsagePartial          bool
}

type directWireUsage struct {
	Total struct {
		TotalTokens           uint64 `json:"totalTokens"`
		InputTokens           uint64 `json:"inputTokens"`
		OutputTokens          uint64 `json:"outputTokens"`
		CachedInputTokens     uint64 `json:"cachedInputTokens"`
		ReasoningOutputTokens uint64 `json:"reasoningOutputTokens"`
	} `json:"total"`
	Last struct {
		TotalTokens uint64 `json:"totalTokens"`
	} `json:"last"`
	ModelContextWindow    uint64   `json:"modelContextWindow"`
	EstimatedCostUSD      *float64 `json:"estimatedCostUsd"`
	CostPartial           bool     `json:"costPartial"`
	OutputTokensPerSecond *float64 `json:"outputTokensPerSecond"`
	WorkDurationMs        *int64   `json:"workDurationMs"`
	WorkStartedAt         *int64   `json:"workStartedAt"`
	LastResponseAt        *int64   `json:"lastResponseAt"`
	LastCompletedAt       *int64   `json:"lastCompletedAt"`
	Turns                 uint64   `json:"turns"`
	Roundtrips            uint64   `json:"roundtrips"`
	RoundtripsPartial     bool     `json:"roundtripsPartial"`
	UsageUnavailable      bool     `json:"usageUnavailable"`
	UsagePartial          bool     `json:"usagePartial"`
}

func (c *directNativeConnection) applyThreadUsageLocked(thread string, wire *directWireUsage) {
	if wire == nil {
		return
	}
	timestamp := func(seconds *int64) time.Time {
		if seconds == nil {
			return time.Time{}
		}
		return time.Unix(*seconds, 0)
	}
	u := directThreadUsage{
		NativeUsage: sessionapi.NativeUsage{
			TotalTokens:           wire.Total.TotalTokens,
			InputTokens:           wire.Total.InputTokens,
			OutputTokens:          wire.Total.OutputTokens,
			CachedInputTokens:     wire.Total.CachedInputTokens,
			ReasoningOutputTokens: wire.Total.ReasoningOutputTokens,
		},
		OutputTokensPerSecond: wire.OutputTokensPerSecond,
		ContextTokens:         wire.Last.TotalTokens,
		ContextWindow:         wire.ModelContextWindow,
		CostPartial:           wire.CostPartial,
		LastResponse:          timestamp(wire.LastResponseAt),
		LastCompleted:         timestamp(wire.LastCompletedAt),
		WorkSince:             timestamp(wire.WorkStartedAt),
		Roundtrips:            wire.Roundtrips,
		RoundtripsPartial:     wire.RoundtripsPartial,
		Turns:                 wire.Turns,
		TokensKnown:           !wire.UsageUnavailable,
		UsagePartial:          wire.UsagePartial,
	}
	if wire.EstimatedCostUSD != nil {
		u.EstimatedCostUSD, u.CostKnown = *wire.EstimatedCostUSD, true
	}
	if wire.WorkDurationMs != nil {
		u.WorkDurationMs, u.WorkKnown = *wire.WorkDurationMs, true
	}
	if c.threadUsage == nil {
		c.threadUsage = make(map[string]directThreadUsage)
	}
	c.threadUsage[thread] = u
}

func (u directThreadUsage) throughputLabel() string {
	if u.OutputTokensPerSecond == nil {
		return ""
	}
	return fmt.Sprintf("%.1f tok/s", *u.OutputTokensPerSecond)
}

type directNativeOptions struct {
	socket                string
	workspace             string
	routerArgs            []string
	args                  []string
	resume                string
	profile               string
	requireCustomProvider bool
	yolo                  bool
	configArgs            []string
}

var directAttachmentSeq atomic.Uint64

// directNativeConnection connects the Go TUI directly to mekugi-app-server's
// Unix-socket WebSocket listener. Native server owners drive transcript,
// roster, command output, saved changes, and prompts; Go retains no session
// state machine or control loop.
type directNativeConnection struct {
	opts       directNativeOptions
	wire       *nativeAppServerWire
	attachment string
	attachID   uint64

	ctx       context.Context
	cancel    context.CancelFunc
	closeOnce sync.Once

	events chan sessionapi.NativeEvent
	errCh  chan error

	mu                 sync.Mutex
	closed             bool
	revision           uint64
	seq                uint64
	mainThread         string
	displayedThread    string
	inputThread        string
	sideThread         string
	sideFirstTurn      string
	activeTurnSince    time.Time
	sideTurnSince      time.Time
	mainTurnStatus     sessionpresentation.Label
	sideTurnStatus     sessionpresentation.Label
	sideReplaceTask    string
	sideModel          string
	sideEffort         string
	btwThread          string
	btwMainThread      string
	btw                conversation.SideAnswer
	btwCompletedAnswer string
	btwRunning         bool
	workspace          string
	title              string
	model              string
	modelProvider      string
	effort             string
	tier               string
	permissionProfile  string
	approvalPolicy     string
	sandboxPolicy      string
	instructionSources []string
	busy               bool
	activeTurn         string
	sideActiveTurn     string
	inputTurnStarts    []*directInputTurnStart
	firstInputs        map[string]directFirstInput
	archived           bool
	threadUsage        map[string]directThreadUsage
	threadSkills       map[string]activity.SkillSet
	threadSkillReads   map[string]activity.SkillSet
	rateLimits         [2]*sessionpresentation.CaptionLimit
	prediction         directPrediction
	feedbackText       string
	feedbackAlert      bool
	feedbackRev        uint64
	retryNotices       map[string]directRetryNotice
	btwRetrying        bool

	items       map[string][]*directItemRecord
	itemsByID   map[directItemKey]*directItemRecord
	mainOutput  []activity.OutputResult
	agentOutput []activity.OutputResult

	family              *sessionapi.Family
	threadCallers       map[string]string
	review              *changes.ReviewView
	reviewRev           uint64
	reviewThread        string
	reviewPending       chan struct{}
	editReceipts        map[nativeEditItem][]nativeCapturedFile
	mainQueue           directQueueState
	sideQueue           directQueueState
	queueRead           uint64
	steerTurns          map[directSteerTurnID]*directSteerTurn
	resumeView          *sessionpresentation.ResumeView
	requests            map[string]*directPendingServerRequest
	requestOrder        []string
	details             *sessionapi.NativeDetails
	detailSeq           uint64
	processRefresh      uint64
	processLists        map[string]bool
	panel               *sessionpresentation.PanelView
	panelKind           string
	panelSeq            uint64
	processes           []sessionpresentation.Process
	live                []preview.Preview
	liveDirty           bool
	liveHidden          bool
	interruptLocked     bool
	predictionsDisabled bool
	preferences         jsontext.Value
	inputHistory        []composer.HistoryEntry
	inputHistoryRev     uint64
	focused             bool
	menu                *sessionpresentation.MenuView
	menuSeq             uint64
	quitOffer           directQuitWork
	settingsMenuThread  string
	completionTarget    conversation.EditorTarget
	dismissedComp       completion.Target
	presetCatalog       *[10]presets.Slot
	presetFlow          *presets.Flow
	skillsCwd           string
	skillsLoaded        bool
	skills              []completion.Choice
	skillsGen           uint64
	skillManager        *directSkillManager
	copyChoices         []copyChoice
	rewind              *directRewindMenu
	effectSeq           uint64
	effects             []sessionapi.NativeEffect
}

func openDirectNativeConnection(ctx context.Context, socket, workspace string, routerArgs, args []string, resume, profile string) (*directNativeConnection, error) {
	parsedRouter, err := launchoptions.Parse(routerArgs)
	if err != nil {
		return nil, err
	}
	configArgs, _, yolo, err := appServerArgs(expandReasoningShortcuts(args))
	if err != nil {
		return nil, err
	}
	opts := directNativeOptions{
		socket:                socket,
		workspace:             workspace,
		routerArgs:            slices.Clone(routerArgs),
		args:                  slices.Clone(args),
		resume:                resume,
		profile:               profile,
		requireCustomProvider: parsedRouter.RequireCustomProvider,
		yolo:                  yolo,
		configArgs:            nativeAppServerArgs(configArgs),
	}
	return newDirectNativeConnection(ctx, opts, "")
}

func newDirectNativeConnection(ctx context.Context, opts directNativeOptions, attachThread string) (*directNativeConnection, error) {
	wire, err := dialNativeAppServer(ctx, opts.socket)
	if err != nil {
		return nil, err
	}
	connCtx, cancel := context.WithCancel(context.Background())
	attachID := directAttachmentSeq.Add(1)
	c := &directNativeConnection{
		opts:          opts,
		wire:          wire,
		attachment:    fmt.Sprintf("direct-%d", attachID),
		attachID:      attachID,
		ctx:           connCtx,
		cancel:        cancel,
		events:        make(chan sessionapi.NativeEvent, 64),
		errCh:         make(chan error, 1),
		reviewPending: make(chan struct{}, 1),
		workspace:     opts.workspace,
		items:         make(map[string][]*directItemRecord),
		itemsByID:     make(map[directItemKey]*directItemRecord),
		firstInputs:   make(map[string]directFirstInput),
		threadCallers: make(map[string]string),
		threadUsage:   make(map[string]directThreadUsage),
		requests:      make(map[string]*directPendingServerRequest),
		focused:       true,
	}

	if err := c.bootstrap(ctx, attachThread); err != nil {
		_ = c.Close()
		return nil, err
	}
	c.mu.Lock()
	c.publishLocked()
	c.mu.Unlock()
	go c.eventLoop()
	go c.reviewLoop()
	return c, nil
}

func (c *directNativeConnection) bootstrap(ctx context.Context, attachThread string) error {
	targetResume := attachThread
	if targetResume == "" {
		targetResume = c.opts.resume
	}
	switch targetResume {
	case "--pick":
		return c.openResumePicker(ctx)
	case "--last":
		listParams := map[string]any{
			"limit": 1, "modelProviders": []string{}, "archived": false,
			"currentAccountOnly": true, "sortKey": "updated_at",
		}
		if c.opts.workspace != "" {
			listParams["cwd"] = c.opts.workspace
		}
		var listed struct {
			Data []struct {
				ID string `json:"id"`
			} `json:"data"`
		}
		if err := c.wire.Call(ctx, "thread/list", listParams, &listed); err != nil {
			return fmt.Errorf("list threads for resume --last: %w", err)
		}
		if len(listed.Data) == 0 {
			return errors.New("no saved threads available to resume")
		}
		targetResume = listed.Data[0].ID
	}

	if targetResume != "" {
		return c.resumeThread(ctx, targetResume)
	}
	return c.startFreshThread(ctx, c.newThreadParams())
}

func (c *directNativeConnection) familyParams() map[string]any {
	if c.opts.profile == "" && !c.opts.requireCustomProvider {
		return nil
	}
	env := make(map[string]string)
	for _, entry := range serverEnvironment(os.Environ()) {
		if k, v, ok := strings.Cut(entry, "="); ok && k != "" {
			env[k] = v
		}
	}
	family := map[string]any{
		"environment": env,
	}
	if c.opts.profile != "" {
		family["profile"] = c.opts.profile
	}
	if c.opts.requireCustomProvider {
		family["requireCustomProvider"] = true
	}
	return family
}

func (c *directNativeConnection) newThreadParams() map[string]any {
	params := map[string]any{
		"cwd": c.opts.workspace,
	}
	if cfg := parseDirectConfigOverrides(c.opts.configArgs); len(cfg) > 0 {
		params["config"] = cfg
	}
	if c.opts.yolo {
		params["approvalPolicy"] = "never"
		params["sandbox"] = "danger-full-access"
	}
	if family := c.familyParams(); family != nil {
		params["family"] = family
	}
	return params
}

func (c *directNativeConnection) startFreshThread(ctx context.Context, params map[string]any) error {
	var resp directThreadStartResponse
	if err := c.wire.Call(ctx, "thread/start", params, &resp); err != nil {
		return fmt.Errorf("start native thread: %w", err)
	}
	c.mu.Lock()
	c.applyThreadBootstrapLocked(resp)
	c.publishLocked()
	c.mu.Unlock()
	c.refreshInputHistory(ctx, resp.Thread.ID)
	c.refreshAuxiliary(ctx, resp.Thread.ID)
	return nil
}

func (c *directNativeConnection) resumeThread(ctx context.Context, threadID string) error {
	params := map[string]any{
		"threadId": threadID,
	}
	// Forward only invocation overrides, not the launch workspace or resolved
	// server defaults. Missing fields remain eligible for native restoration.
	overrides := appServerResumeConfig(c.opts.configArgs, nil)
	for configKey, wireKey := range map[string]string{
		"model": "model", "model_provider": "modelProvider", "service_tier": "serviceTier",
	} {
		if value, explicit := overrides[configKey]; explicit {
			params[wireKey] = value
			delete(overrides, configKey)
		}
	}
	if len(overrides) != 0 {
		params["config"] = overrides
	}
	if c.opts.yolo {
		params["approvalPolicy"] = "never"
		params["sandbox"] = "danger-full-access"
	}
	if family := c.familyParams(); family != nil {
		params["family"] = family
	}
	var resp directThreadStartResponse
	if err := c.wire.Call(ctx, "thread/resume", params, &resp); err != nil {
		return fmt.Errorf("resume native thread %s: %w", threadID, err)
	}
	c.mu.Lock()
	c.applyThreadBootstrapLocked(resp)
	c.hydrateThreadTurnsLocked(resp.Thread.ID, resp.Thread.Turns)
	c.mu.Unlock()
	c.refreshInputHistory(ctx, resp.Thread.ID)
	c.refreshAuxiliary(ctx, resp.Thread.ID)
	return nil
}

func (c *directNativeConnection) applyThreadBootstrapLocked(response directThreadStartResponse) {
	threadID, cwd, name := response.Thread.ID, cmp.Or(response.Cwd, response.Thread.Cwd), response.Thread.Name
	model, effort, tier := response.Model, response.ReasoningEffort, response.ServiceTier
	previousThread, previousWorkspace := c.mainThread, c.workspace
	if c.mainThread != threadID {
		c.resetPredictionLocked("")
		c.activeTurn = ""
		c.mainTurnStatus = sessionpresentation.Label{}
		c.busy = c.sideActiveTurn != ""
		c.archived = false
		c.title = name
		c.modelProvider = ""
		c.effort = ""
		c.tier = ""
		c.family, c.review = nil, nil
		c.reviewThread, c.editReceipts = "", nil
		c.mainQueue = directQueueState{}
		c.processes = nil
		c.processLists = nil
		if c.details != nil && c.details.View.Processes {
			c.details = nil
		}
		c.presetCatalog = nil
		c.clearMenuLocked()
		c.dismissedComp = completion.Target{}
	}
	c.mainThread = threadID
	c.displayedThread = threadID
	c.inputThread = threadID
	c.applyThreadUsageLocked(threadID, response.Thread.TokenUsage)
	c.observeThreadSkillsLocked(threadID, response.Thread.Extra.Skills)
	if cwd != "" {
		c.workspace = cwd
	}
	if threadID != previousThread || c.workspace != previousWorkspace {
		c.loadTerminalPreferencesLocked()
	}
	c.permissionProfile = ""
	c.approvalPolicy = directApprovalLabel(response.ApprovalPolicy)
	c.sandboxPolicy = response.Sandbox.Type
	c.instructionSources = response.InstructionSources
	if response.ActivePermissionProfile != nil {
		c.permissionProfile = response.ActivePermissionProfile.ID
	}
	if name != "" {
		c.title = name
	}
	if model != "" {
		c.model = model
	}
	if response.ModelProvider != "" {
		c.modelProvider = response.ModelProvider
	}
	if effort != "" {
		c.effort = effort
	}
	if tier != "" {
		c.tier = tier
	}
	c.threadCallers[threadID] = "/root"
	c.resumeView = nil
	if turns := response.Thread.Turns; len(turns) != 0 {
		last := turns[len(turns)-1]
		if last.Status == "inProgress" {
			c.setActiveInputTurnLocked(threadID, last.ID, last.StartedAt)
		} else if c.activeTurn == "" {
			c.mainTurnStatus = directFinishedTurnStatus(last.Status, last.DurationMS)
			if last.Status == "completed" {
				c.resetPredictionLocked(last.ID)
			}
		}
	}
}

func (c *directNativeConnection) refreshAuxiliary(ctx context.Context, threadID string) {
	if threadID == "" {
		return
	}
	c.refreshSubtree(ctx, threadID)
	c.requestChanges("")
	c.refreshQueue(ctx, threadID)
	c.refreshRateLimits(ctx)
}

func (c *directNativeConnection) Next() (sessionapi.NativeEvent, error) {
	select {
	case ev, ok := <-c.events:
		if !ok {
			select {
			case err := <-c.errCh:
				return sessionapi.NativeEvent{}, err
			default:
				return sessionapi.NativeEvent{}, io.EOF
			}
		}
		return ev, nil
	case err := <-c.errCh:
		return sessionapi.NativeEvent{}, err
	}
}

func (c *directNativeConnection) Attach(ctx context.Context, thread string) (nativeSessionConnection, error) {
	return newDirectNativeConnection(ctx, c.opts, thread)
}

func (c *directNativeConnection) Close() error {
	var err error
	c.closeOnce.Do(func() {
		c.cancel()
		err = c.wire.Close()
	})
	return err
}

func (c *directNativeConnection) finish(err error) {
	select {
	case c.errCh <- err:
	default:
	}
	_ = c.Close()
}

func (c *directNativeConnection) eventLoop() {
	defer func() {
		c.mu.Lock()
		c.closed = true
		close(c.events)
		c.mu.Unlock()
	}()
	for {
		select {
		case <-c.ctx.Done():
			return
		case msg, ok := <-c.wire.Events():
			if !ok {
				select {
				case c.errCh <- cmp.Or(c.wire.readError(), io.EOF):
				default:
				}
				return
			}
			c.handleServerMessage(msg)
		}
	}
}

func (c *directNativeConnection) publishLocked() {
	if c.closed {
		return
	}
	c.revision++
	ev := c.buildEventLocked()
	select {
	case c.events <- ev:
	default:
		select {
		case <-c.events:
		default:
		}
		select {
		case c.events <- ev:
		default:
		}
	}
}

func (c *directNativeConnection) buildEventLocked() sessionapi.NativeEvent {
	displayed := c.displayedThread
	if displayed == "" {
		displayed = c.mainThread
	}
	input := c.inputThread
	if input == "" {
		input = c.mainThread
	}
	queue := c.mainQueue
	if input == c.sideThread && input != "" {
		queue = c.sideQueue
	}
	mainCaller := c.threadCallers[c.mainThread]
	if mainCaller == "" {
		mainCaller = "/root"
	}
	displayedCaller := c.threadCallers[displayed]
	if displayedCaller == "" {
		displayedCaller = mainCaller
	}
	inputCaller := c.threadCallers[input]
	if inputCaller == "" {
		inputCaller = mainCaller
	}
	var sideThread sessionpresentation.Thread
	if c.sideThread != "" {
		sideThread = sessionpresentation.Thread{ID: c.sideThread, Caller: sessionpresentation.SideCaller}
	}

	mainUsage := c.threadUsage[c.mainThread]
	displayedUsage := c.threadUsage[displayed]
	captionCtxTokens := mainUsage.ContextTokens
	captionCtxWindow := mainUsage.ContextWindow
	if displayed == c.sideThread || displayedUsage.ContextWindow > 0 || displayedUsage.ContextTokens > 0 {
		captionCtxTokens = displayedUsage.ContextTokens
		captionCtxWindow = displayedUsage.ContextWindow
	}
	var captionItems []presentation.Item
	captionModel, captionEffort := c.model, c.effort
	if displayed == c.sideThread && c.sideThread != "" {
		captionModel, captionEffort = c.sideModel, c.sideEffort
	}
	if label := displayedUsage.throughputLabel(); label != "" {
		captionItems = append(captionItems, presentation.Item{
			ID:   usagepresentation.Throughput.ID,
			Text: label,
		})
	}

	composerStatus := c.mainTurnStatus
	since := c.activeTurnSince
	if input == c.sideThread && c.sideThread != "" {
		composerStatus, since = c.sideTurnStatus, c.sideTurnSince
	}
	if c.activeTurnForLocked(input) != "" {
		composerStatus = sessionpresentation.Label{Text: "Working", Running: true, Started: since}
	}
	pending := conversation.PendingView{
		CanSteer: c.activeTurnForLocked(input) != "" && len(queue.waiting) > 0,
		Waiting:  c.pendingSteersLocked(input),
		Queued:   slices.Clone(queue.pending),
	}
	if len(pending.Waiting) != 0 {
		pending.Reason = conversation.Steering
	}
	view := sessionpresentation.View{
		DisplayedWorkspace: c.workspace,
		Info: sessionpresentation.MainInfo{
			Workspace:      c.workspace,
			Skills:         c.loadedSkillsLocked(c.mainThread),
			Title:          c.title,
			BackendVersion: c.wire.UserAgent(),
			Caller:         mainCaller,
		},
		Resume:    c.resumeView,
		Menu:      cloneMenuView(c.menu),
		Panel:     clonePanelView(c.panel),
		Version:   sessionpresentation.Version{Attachment: c.attachID, Revision: c.revision},
		Main:      sessionpresentation.Thread{ID: c.mainThread, Caller: mainCaller},
		Displayed: sessionpresentation.Thread{ID: displayed, Caller: displayedCaller},
		Input:     sessionpresentation.Thread{ID: input, Caller: inputCaller},
		Status: sessionpresentation.MainStatus{
			State: sessionpresentation.Label{
				Text:    c.statusTextLocked(),
				Running: c.busy,
			},
			Session:   composerStatus,
			Composer:  composerStatus,
			Processes: len(c.processes),
			Animating: c.busy || len(c.processes) > 0,
		},
		Caption: sessionpresentation.Caption{
			Model:         captionModel,
			Effort:        captionEffort,
			Tier:          c.tier,
			ContextTokens: captionCtxTokens,
			ContextWindow: captionCtxWindow,
			ContextKnown:  captionCtxWindow > 0,
			InputTokens:   displayedUsage.InputTokens,
			OutputTokens:  displayedUsage.OutputTokens,
			UsagePartial:  displayedUsage.UsagePartial,
			Cost:          displayedUsage.EstimatedCostUSD,
			CostKnown:     displayedUsage.CostKnown,
			CostPartial:   displayedUsage.CostPartial,
			Items:         captionItems,
			Limits:        directCaptionLimits(c.rateLimits),
		},
		Pending: pending,
		Draft: conversation.DraftView{
			Available:  c.mainThread != "" && c.resumeView == nil,
			Prediction: c.prediction.text,
		},
		Interrupt:          sessioncontrol.InterruptTarget{Thread: input, Turn: c.activeTurnForLocked(input)},
		InterruptAvailable: c.busy || c.btwRunning,
		SideThread:         sideThread,
		BTWRunning:         c.btwRunning,
	}
	if c.displayedThread != "" && c.displayedThread != c.mainThread && c.displayedThread != c.sideThread {
		view.ReadOnly = sessionpresentation.ChildDisplayed
	}
	if c.feedbackText != "" {
		view.Status.SetFeedback(sessionpresentation.Label{Text: c.feedbackText, Alert: c.feedbackAlert}, c.feedbackRev)
	}
	if c.sideThread != "" {
		view.Side = &sessionpresentation.SideToken{
			ID: 1, Key: c.sideThread, Label: "One-off", Skills: c.loadedSkillsLocked(c.sideThread),
		}
	}
	if c.btwThread != "" {
		view.BTW = &conversation.SideView{
			Key:       c.btwThread,
			ID:        c.btw.ID,
			Question:  c.btw.Question,
			History:   slices.Clone(c.btw.History),
			Status:    c.btw.Status,
			Answer:    slices.Clone(c.btw.Answer),
			Alert:     c.btw.Alert,
			Truncated: c.btw.Truncated,
		}
	}

	var approvals []prompt.ApprovalView
	var questions []prompt.CallView
	for _, id := range c.requestOrder {
		req := c.requests[id]
		if req == nil {
			continue
		}
		if req.approval != nil {
			approvals = append(approvals, *req.approval)
		}
		if req.question != nil {
			questions = append(questions, *req.question)
		}
	}

	mainPub := c.publicationForThreadLocked(c.mainThread)
	agentsPub := mainPub
	if displayed != c.mainThread {
		agentsPub = c.publicationForThreadLocked(displayed)
	}
	var family *sessionapi.Family
	if c.family != nil {
		cp := *c.family
		cp.Agents = &activity.Publication{
			Roster: agentsPub.Roster, LineCounts: agentsPub.LineCounts, NetCounts: agentsPub.NetCounts,
		}
		cp.Status = sessionpresentation.Label{Text: c.statusTextLocked(), Running: c.busy}
		family = &cp
	}

	var liveScope preview.Scope
	if c.liveDirty && c.workspace != "" {
		threads := map[string]bool{displayed: true}
		if family != nil && family.Root == displayed {
			for _, thread := range family.Threads {
				threads[thread] = true
			}
		}
		liveScope = preview.Scope{
			Workspaces: map[string]map[string]bool{c.workspace: threads},
		}
	}

	var review *changes.ReviewView
	if c.reviewThread == displayed {
		review = c.review
	}
	predictionTurn := ""
	if !c.prediction.pending && !c.prediction.attempted && c.predictionAllowedLocked(c.mainThread, c.prediction.turn) {
		predictionTurn = c.prediction.turn
	}
	frame := sessionapi.NativeFrame{
		Processes:         slices.Clone(c.processes),
		Questions:         questions,
		Approvals:         approvals,
		View:              view,
		Main:              mainPub,
		Agents:            agentsPub,
		Family:            family,
		Review:            review,
		Live:              slices.Clone(c.live),
		LiveScope:         liveScope,
		Details:           c.details,
		MainOutput:        slices.Clone(c.mainOutput),
		AgentOutput:       slices.Clone(c.agentOutput),
		Effects:           slices.Clone(c.effects),
		CompletionTarget:  c.completionTarget,
		CompletionCatalog: c.skillsGen,
		Facts: sessionapi.NativeSessionFacts{
			History:            c.inputHistory,
			HistoryRevision:    c.inputHistoryRev,
			Preferences:        c.preferences,
			Usage:              mainUsage.NativeUsage,
			Archived:           c.archived,
			PredictionTurn:     predictionTurn,
			PredictionRevision: c.prediction.revision,
			ActionRequired:     len(approvals) > 0 || len(questions) > 0,
			Busy:               c.busy,
		},
	}
	return sessionapi.NativeEvent{
		Attachment: c.attachment,
		Frame:      frame,
	}
}

type directRetryNotice struct {
	threadID string
	turnID   string
	text     string
}

func (c *directNativeConnection) statusTextLocked() string {
	for _, id := range []string{cmp.Or(c.displayedThread, c.mainThread), c.mainThread, c.sideThread} {
		if notice, ok := c.retryNotices[id]; ok && notice.text != "" {
			return notice.text
		}
	}
	if c.busy {
		return "Working"
	}
	if c.mainThread == "" {
		return "Connecting…"
	}
	return "Ready"
}

func (c *directNativeConnection) activeTurnForLocked(threadID string) string {
	if threadID != "" && threadID == c.sideThread {
		return c.sideActiveTurn
	}
	return c.activeTurn
}

func (c *directNativeConnection) publicationForThreadLocked(threadID string) *activity.Publication {
	records := c.items[threadID]
	entries := make([]activity.Entry, 0, len(records))
	var lastSeq uint64
	if pending, ok := c.firstInputs[threadID]; ok {
		entries = append(entries, pending.entry)
		lastSeq = pending.entry.Seq
	}
	for _, rec := range records {
		entry := rec.entry
		entry.Spans = slices.Clone(entry.Spans)
		entry.Images = slices.Clone(entry.Images)
		entry.Outputs = maps.Clone(entry.Outputs)
		if last := len(entry.Values) - 1; rec.itemType == "commandExecution" && last > 0 && entry.Values[last].BatchExit {
			// Keep captured changes after the invocation's steps without painting its output row.
			entry.Values = slices.Clone(entry.Values[:last])
			entry.Values[last-1].Changes = rec.entry.Values[last].Changes
		} else {
			entry.Values = slices.Clone(entry.Values)
		}
		entries = append(entries, entry)
		if entry.Seq > lastSeq {
			lastSeq = entry.Seq
		}
	}
	pub := &activity.Publication{
		Entries: entries,
		LastSeq: lastSeq,
		Roster:  c.rosterLocked(),
	}
	if c.review != nil && c.reviewThread == threadID {
		pub.LineCounts, pub.NetCounts = c.review.Counts.Callers, c.review.Counts.Net
	}
	return pub
}

func (c *directNativeConnection) rosterLocked() []activity.AgentRow {
	var rows []activity.AgentRow
	if c.family != nil && c.family.Agents != nil && len(c.family.Agents.Roster) > 0 {
		rows = slices.Clone(c.family.Agents.Roster)
	} else if c.mainThread != "" {
		caller := c.threadCallers[c.mainThread]
		if caller == "" {
			caller = "/root"
		}
		rows = []activity.AgentRow{{
			Agent: activity.Agent{
				AgentFacts: activity.AgentFacts{
					Name:   caller,
					Thread: c.mainThread,
				},
			},
		}}
	}
	for i := range rows {
		agent := &rows[i].Agent
		if agent.Thread == c.mainThread && c.activeTurn != "" {
			agent.Responding = true
		}
		if c.hasPendingRequestForThreadLocked(agent.Thread) {
			agent.Waiting = true
		}
		agent.Status = agent.ObservedStatus(agent.LatestKind)
		agent.Skills = c.loadedSkillsLocked(agent.Thread)
		if u, ok := c.threadUsage[agent.Thread]; ok {
			agent.InputTokens = u.InputTokens
			agent.OutputTokens = u.OutputTokens
			agent.TokensKnown = u.TokensKnown
			agent.UsagePartial = u.UsagePartial
			agent.ContextTokens = u.ContextTokens
			agent.ContextWindow = u.ContextWindow
			agent.ContextKnown = u.ContextWindow > 0
			agent.Cost = u.EstimatedCostUSD
			agent.CostKnown = u.CostKnown
			agent.CostPartial = u.CostPartial
			agent.Roundtrips = u.Roundtrips
			agent.RoundtripsPartial = u.RoundtripsPartial
			agent.Turns = u.Turns
			agent.LastResponse = u.LastResponse
			if u.WorkKnown {
				agent.WorkElapsed = time.Duration(u.WorkDurationMs) * time.Millisecond
				agent.WorkKnown = true
			}
			if agent.Responding {
				agent.WorkSince = u.WorkSince
			}
			if label := u.throughputLabel(); label != "" {
				agent.StatusItems = []presentation.Item{{
					ID:   usagepresentation.Throughput.ID,
					Text: label,
				}}
			}
		}
	}
	return rows
}

func parseDirectConfigOverrides(args []string) map[string]any {
	cfg := make(map[string]any)
	for i := 0; i < len(args); i++ {
		if (args[i] == "-c" || args[i] == "--config") && i+1 < len(args) {
			i++
			k, v, ok := strings.Cut(args[i], "=")
			if !ok || k == "" {
				continue
			}
			v = strings.TrimSpace(v)
			switch {
			case v == "true":
				cfg[k] = true
			case v == "false":
				cfg[k] = false
			case len(v) >= 2 && v[0] == '"' && v[len(v)-1] == '"':
				cfg[k] = v[1 : len(v)-1]
			default:
				cfg[k] = v
			}
		}
	}
	return cfg
}

type directThreadStartResponse struct {
	Thread                  directWireThread         `json:"thread"`
	Model                   string                   `json:"model"`
	ModelProvider           string                   `json:"modelProvider"`
	ReasoningEffort         string                   `json:"reasoningEffort"`
	ServiceTier             string                   `json:"serviceTier"`
	ActivePermissionProfile *directPermissionProfile `json:"activePermissionProfile"`
	ApprovalPolicy          jsontext.Value           `json:"approvalPolicy"`
	Sandbox                 directSandboxPolicy      `json:"sandbox"`
	InstructionSources      []string                 `json:"instructionSources"`
	Cwd                     string                   `json:"cwd"`
}

type directSandboxPolicy struct {
	Type string `json:"type"`
}

func directApprovalLabel(policy jsontext.Value) string {
	var name string
	if json.Unmarshal(policy, &name) == nil {
		return name
	}
	return string(policy)
}

type directWireThread struct {
	ID        string `json:"id"`
	Name      string `json:"name"`
	Preview   string `json:"preview"`
	Cwd       string `json:"cwd"`
	UpdatedAt int64  `json:"updatedAt"`
	GitInfo   struct {
		Branch string `json:"branch"`
	} `json:"gitInfo"`
	ParentThreadID string           `json:"parentThreadId"`
	Source         jsontext.Value   `json:"source"`
	AgentNickname  string           `json:"agentNickname"`
	AgentRole      string           `json:"agentRole"`
	AgentStatus    jsontext.Value   `json:"agentStatus"`
	ThreadSource   string           `json:"threadSource"`
	Status         jsontext.Value   `json:"status"`
	Turns          []directWireTurn `json:"turns"`
	TokenUsage     *directWireUsage `json:"tokenUsage"`
	Extra          struct {
		Skills []string `json:"skills"`
	} `json:"extra"`
}

type directWireTurn struct {
	ID         string             `json:"id"`
	Status     string             `json:"status"`
	Items      []jsontext.Value   `json:"items"`
	Error      *binding.TurnError `json:"error"`
	StartedAt  *int64             `json:"startedAt"`
	DurationMS *int64             `json:"durationMs"`
}
