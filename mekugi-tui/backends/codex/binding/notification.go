package binding

import (
	"encoding/json/jsontext"

	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
)

// Source: apps/mekugi/internal/router/app_server_events.go:67:122@1bcbdd5a.
type TokenUsage struct {
	TotalTokens           uint64 `json:"totalTokens"`
	InputTokens           uint64 `json:"inputTokens"`
	OutputTokens          uint64 `json:"outputTokens"`
	CachedInputTokens     uint64 `json:"cachedInputTokens"`
	ReasoningOutputTokens uint64 `json:"reasoningOutputTokens"`
}

type Turn struct {
	ID         string     `json:"id"`
	Status     string     `json:"status"`
	Error      *TurnError `json:"error"`
	DurationMS *int64     `json:"durationMs"`
}

type TurnError struct {
	Message           string         `json:"message"`
	AdditionalDetails string         `json:"additionalDetails"`
	CodexErrorInfo    jsontext.Value `json:"codexErrorInfo"`
}

// ResponseCompleted is the native completion observation, not context usage.
type ResponseCompleted struct {
	ThreadID          string `json:"threadId"`
	TurnID            string `json:"turnId"`
	ResponseID        string `json:"responseId"`
	Model             string `json:"model"`
	ServiceTier       string `json:"serviceTier"`
	OpenAIEndpoint    bool   `json:"openaiEndpoint"`
	RequestDurationMS int64  `json:"requestDurationMs"`
	Usage             *struct {
		TokenUsage
		CacheWriteInputTokens uint64 `json:"cacheWriteInputTokens"`
	} `json:"usage"`
	UsageMetadata *struct {
		Metadata struct {
			Input struct {
				Cached *uint64 `json:"cached_tokens"`
			} `json:"input_tokens_details"`
			Output struct {
				Reasoning *uint64 `json:"reasoning_tokens"`
			} `json:"output_tokens_details"`
		} `json:"metadata"`
	} `json:"usageMetadata"`
}

type Notification struct {
	Command *CommandObservation `json:"-"`
	Status  struct {
		Type string `json:"type"`
	} `json:"status"`
	ThreadID   string             `json:"threadId"`
	TurnID     string             `json:"turnId"`
	ItemID     string             `json:"itemId"`
	Delta      string             `json:"delta"`
	ProcessID  string             `json:"processId"`
	Stdin      string             `json:"stdin"`
	Item       ThreadItem         `json:"item"`
	Run        *appserver.HookRun `json:"run,omitempty"`
	Thread     ThreadInfo         `json:"thread"`
	TokenUsage struct {
		Total              TokenUsage  `json:"total"`
		Last               *TokenUsage `json:"last"`
		ModelContextWindow uint64      `json:"modelContextWindow"`
	} `json:"tokenUsage"`
	Turn      Turn       `json:"turn"`
	Error     *TurnError `json:"error"`
	WillRetry bool       `json:"willRetry"`
}
