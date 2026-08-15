// Mock OpenAI-compatible upstream for gateway proxy-overhead benchmarking.
//
// It answers two OpenAI wire shapes with a fixed, FULLY-CONFORMANT body as
// fast as possible, so a load test measures the *gateway's* overhead rather
// than real-provider network jitter:
//
//	POST /v1/chat/completions  -> OpenAI Chat Completions JSON
//	POST /v1/responses         -> OpenAI Responses JSON
//	POST /responses, POST /    -> aliases of /v1/responses (ZeroRouter's
//	                              base-url override posts to the exact URL set)
//	GET  /healthz              -> 200 ok
//
// FULL FIDELITY IS THE POINT. The first prototype of this mock returned a
// minimal body (no `created_at`, no `refusal`/`logprobs`/`annotations`, no
// token-detail sub-objects), and Bifrost's Responses->ChatCompletion mapping
// silently produced `choices: null` while still returning HTTP 200 — which
// made its benchmark numbers unverifiable as end-to-end proxy work. Every
// response below carries the complete field set the real OpenAI API returns
// (per the API reference as of 2026-08), and the benchmark harness refuses to
// run until each gateway's PARSED output round-trips the reply text
// (`run.sh verify`).
//
// Knobs (env):
//
//	MOCK_PORT       listen port                       (default 9010)
//	MOCK_DELAY_MS   fixed sleep before responding     (default 0)
//	MOCK_IN_TOKENS  reported input/prompt tokens      (default 12)
//	MOCK_OUT_TOKENS reported output/completion tokens (default 9)
//	MOCK_LOG        "1" logs each request path        (default off)
//
// Streaming: if the request body has "stream": true, the same content is
// emitted as Server-Sent Events in the matching wire's full event ceremony
// (event: lines, sequence numbers, terminal usage).
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"strconv"
	"time"
)

const replyText = "Hello! How can I help you today?"

var (
	delay     time.Duration
	inTokens  int
	outTokens int
	logReqs   bool
	startUnix = time.Now().Unix()
)

type streamOptions struct {
	IncludeUsage bool `json:"include_usage"`
}

type reqEnvelope struct {
	Model         string         `json:"model"`
	Stream        bool           `json:"stream"`
	StreamOptions *streamOptions `json:"stream_options"`
}

func decodeReq(r *http.Request) reqEnvelope {
	var e reqEnvelope
	// Read + decode; ignore errors (benchmark bodies are always well-formed).
	body, _ := io.ReadAll(r.Body)
	_ = json.Unmarshal(body, &e)
	if logReqs {
		fmt.Printf("%s %s model=%q stream=%v\n", r.Method, r.URL.Path, e.Model, e.Stream)
	}
	return e
}

func maybeDelay() {
	if delay > 0 {
		time.Sleep(delay)
	}
}

// ---- OpenAI Chat Completions ------------------------------------------------

// chatUsage is the complete usage object the real API returns, including both
// token-detail sub-objects.
func chatUsage() map[string]any {
	return map[string]any{
		"prompt_tokens":     inTokens,
		"completion_tokens": outTokens,
		"total_tokens":      inTokens + outTokens,
		"prompt_tokens_details": map[string]any{
			"cached_tokens": 0,
			"audio_tokens":  0,
		},
		"completion_tokens_details": map[string]any{
			"reasoning_tokens":           0,
			"audio_tokens":               0,
			"accepted_prediction_tokens": 0,
			"rejected_prediction_tokens": 0,
		},
	}
}

func chatCompletionsHandler(w http.ResponseWriter, r *http.Request) {
	e := decodeReq(r)
	maybeDelay()
	model := e.Model
	if model == "" {
		model = "mock-model"
	}
	if e.Stream {
		streamChat(w, model, e.StreamOptions != nil && e.StreamOptions.IncludeUsage)
		return
	}
	resp := map[string]any{
		"id":      "chatcmpl-mockbench0000000000000000",
		"object":  "chat.completion",
		"created": startUnix,
		"model":   model,
		"choices": []any{map[string]any{
			"index": 0,
			"message": map[string]any{
				"role":        "assistant",
				"content":     replyText,
				"refusal":     nil,
				"annotations": []any{},
			},
			"logprobs":      nil,
			"finish_reason": "stop",
		}},
		"usage":              chatUsage(),
		"service_tier":       "default",
		"system_fingerprint": "fp_mockbench",
	}
	writeJSON(w, resp)
}

func streamChat(w http.ResponseWriter, model string, includeUsage bool) {
	f, ok := w.(http.Flusher)
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.WriteHeader(http.StatusOK)
	chunk := func(delta map[string]any, finish any, usage any) {
		c := map[string]any{
			"id":                 "chatcmpl-mockbench0000000000000000",
			"object":             "chat.completion.chunk",
			"created":            startUnix,
			"model":              model,
			"service_tier":       "default",
			"system_fingerprint": "fp_mockbench",
			"choices": []any{map[string]any{
				"index":         0,
				"delta":         delta,
				"logprobs":      nil,
				"finish_reason": finish,
			}},
		}
		if usage != nil {
			// The real API sends `usage: null` on every chunk once
			// stream_options.include_usage is set, then the totals on a final
			// chunk with an empty choices array. Passing usage here replaces
			// the choices with that final empty array.
			c["choices"] = []any{}
			c["usage"] = usage
		} else if includeUsage {
			c["usage"] = nil
		}
		b, _ := json.Marshal(c)
		fmt.Fprintf(w, "data: %s\n\n", b)
		if ok {
			f.Flush()
		}
	}
	chunk(map[string]any{"role": "assistant", "content": "", "refusal": nil}, nil, nil)
	chunk(map[string]any{"content": replyText}, nil, nil)
	chunk(map[string]any{}, "stop", nil)
	if includeUsage {
		chunk(nil, nil, chatUsage())
	}
	fmt.Fprintf(w, "data: [DONE]\n\n")
	if ok {
		f.Flush()
	}
}

// ---- OpenAI Responses (ZeroRouter openai_responses wire; Bifrost's OpenAI
// provider also dials this API) --------------------------------------------

func responsesUsage() map[string]any {
	return map[string]any{
		"input_tokens":          inTokens,
		"input_tokens_details":  map[string]any{"cached_tokens": 0},
		"output_tokens":         outTokens,
		"output_tokens_details": map[string]any{"reasoning_tokens": 0},
		"total_tokens":          inTokens + outTokens,
	}
}

func responsesMessageItem() map[string]any {
	return map[string]any{
		"id":     "msg_mockbench00000000000000000000",
		"type":   "message",
		"status": "completed",
		"role":   "assistant",
		"content": []any{map[string]any{
			"type":        "output_text",
			"annotations": []any{},
			"logprobs":    []any{},
			"text":        replyText,
		}},
	}
}

// responseObject is the complete Responses-API response resource. `status`,
// `output`, and `usage` vary between the in-progress (streaming) and
// completed forms.
func responseObject(model, status string, output []any, usage any) map[string]any {
	return map[string]any{
		"id":                   "resp_mockbench0000000000000000000",
		"object":               "response",
		"created_at":           startUnix,
		"status":               status,
		"background":           false,
		"error":                nil,
		"incomplete_details":   nil,
		"instructions":         nil,
		"max_output_tokens":    nil,
		"max_tool_calls":       nil,
		"model":                model,
		"output":               output,
		"parallel_tool_calls":  true,
		"previous_response_id": nil,
		"prompt_cache_key":     nil,
		"reasoning":            map[string]any{"effort": nil, "summary": nil},
		"safety_identifier":    nil,
		"service_tier":         "default",
		"store":                false,
		"temperature":          1.0,
		"text":                 map[string]any{"format": map[string]any{"type": "text"}},
		"tool_choice":          "auto",
		"tools":                []any{},
		"top_logprobs":         0,
		"top_p":                1.0,
		"truncation":           "disabled",
		"usage":                usage,
		"user":                 nil,
		"metadata":             map[string]any{},
	}
}

func responsesHandler(w http.ResponseWriter, r *http.Request) {
	e := decodeReq(r)
	maybeDelay()
	model := e.Model
	if model == "" {
		model = "mock-model"
	}
	if e.Stream {
		streamResponses(w, model)
		return
	}
	writeJSON(w, responseObject(model, "completed", []any{responsesMessageItem()}, responsesUsage()))
}

func streamResponses(w http.ResponseWriter, model string) {
	f, ok := w.(http.Flusher)
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.WriteHeader(http.StatusOK)
	seq := 0
	emit := func(event string, v map[string]any) {
		v["type"] = event
		v["sequence_number"] = seq
		seq++
		b, _ := json.Marshal(v)
		fmt.Fprintf(w, "event: %s\ndata: %s\n\n", event, b)
		if ok {
			f.Flush()
		}
	}
	inProgress := func() map[string]any {
		return responseObject(model, "in_progress", []any{}, nil)
	}
	itemID := "msg_mockbench00000000000000000000"
	emit("response.created", map[string]any{"response": inProgress()})
	emit("response.in_progress", map[string]any{"response": inProgress()})
	emit("response.output_item.added", map[string]any{
		"output_index": 0,
		"item": map[string]any{
			"id": itemID, "type": "message", "status": "in_progress",
			"role": "assistant", "content": []any{},
		},
	})
	part := map[string]any{"type": "output_text", "annotations": []any{}, "logprobs": []any{}, "text": ""}
	emit("response.content_part.added", map[string]any{
		"item_id": itemID, "output_index": 0, "content_index": 0, "part": part,
	})
	emit("response.output_text.delta", map[string]any{
		"item_id": itemID, "output_index": 0, "content_index": 0,
		"delta": replyText, "logprobs": []any{},
	})
	emit("response.output_text.done", map[string]any{
		"item_id": itemID, "output_index": 0, "content_index": 0,
		"text": replyText, "logprobs": []any{},
	})
	donePart := map[string]any{"type": "output_text", "annotations": []any{}, "logprobs": []any{}, "text": replyText}
	emit("response.content_part.done", map[string]any{
		"item_id": itemID, "output_index": 0, "content_index": 0, "part": donePart,
	})
	emit("response.output_item.done", map[string]any{
		"output_index": 0, "item": responsesMessageItem(),
	})
	emit("response.completed", map[string]any{
		"response": responseObject(model, "completed", []any{responsesMessageItem()}, responsesUsage()),
	})
}

// ---- helpers ----------------------------------------------------------------

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	b, _ := json.Marshal(v)
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(b)
}

func envInt(key string, def int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}

func main() {
	port := envInt("MOCK_PORT", 9010)
	delay = time.Duration(envInt("MOCK_DELAY_MS", 0)) * time.Millisecond
	inTokens = envInt("MOCK_IN_TOKENS", 12)
	outTokens = envInt("MOCK_OUT_TOKENS", 9)
	logReqs = os.Getenv("MOCK_LOG") == "1"

	mux := http.NewServeMux()
	mux.HandleFunc("/v1/chat/completions", chatCompletionsHandler)
	mux.HandleFunc("/v1/responses", responsesHandler)
	mux.HandleFunc("/responses", responsesHandler)
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		// UNKNOWN PATHS FAIL LOUDLY. The prototype aliased any POST here to
		// the Responses handler, and that silent forgiveness hid a real
		// mis-wiring: Bifrost appends `/v1/chat/completions` to its
		// configured base_url, so a base_url ending in `/v1` produced
		// `POST /v1/v1/chat/completions`, this catch-all answered it with a
		// RESPONSES body, and Bifrost — expecting a chat completion —
		// surfaced `choices: null` while still returning HTTP 200. A wrong
		// URL must be a 404 the harness can see, never a wrong-wire 200.
		fmt.Printf("UNEXPECTED %s %s -> 404\n", r.Method, r.URL.Path)
		http.NotFound(w, r)
	})
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "ok")
	})

	srv := &http.Server{
		Addr:         fmt.Sprintf("127.0.0.1:%d", port),
		Handler:      mux,
		WriteTimeout: 30 * time.Second,
		ReadTimeout:  30 * time.Second,
	}
	fmt.Printf("mock-upstream listening on 127.0.0.1:%d delay=%s in=%d out=%d\n", port, delay, inTokens, outTokens)
	if err := srv.ListenAndServe(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
