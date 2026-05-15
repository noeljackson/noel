package agent

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestValidateToolArgumentsObjectValidation(t *testing.T) {
	schema := json.RawMessage(`{
		"type":"object",
		"properties":{"path":{"type":"string"},"timeout_ms":{"type":"number"}},
		"required":["path"],
		"additionalProperties":false
	}`)

	if err := ValidateToolArguments(schema, json.RawMessage(`{"timeout_ms":10}`)); err == nil || !strings.Contains(err.Error(), `"path"`) {
		t.Fatalf("expected missing path error, got %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`{"path":"x","extra":true}`)); err == nil || !strings.Contains(err.Error(), `"extra"`) {
		t.Fatalf("expected extra field error, got %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`{"path":1}`)); err != nil {
		t.Fatalf("number should coerce to string: %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`{"path":"x","timeout_ms":[]}`)); err == nil || !strings.Contains(err.Error(), `"timeout_ms"`) {
		t.Fatalf("expected type mismatch error, got %v", err)
	}
}

func TestValidateToolArgumentsPrimitiveCoercion(t *testing.T) {
	schema := json.RawMessage(`{
		"type":"object",
		"properties":{"timeout_ms":{"type":"number"},"replace_all":{"type":"boolean"}},
		"required":["timeout_ms","replace_all"],
		"additionalProperties":false
	}`)

	coerced, err := ValidateAndCoerceToolArguments(schema, json.RawMessage(`{"timeout_ms":"250","replace_all":"true"}`))
	if err != nil {
		t.Fatalf("expected coercion to pass: %v", err)
	}
	var args struct {
		TimeoutMS  float64 `json:"timeout_ms"`
		ReplaceAll bool    `json:"replace_all"`
	}
	if err := json.Unmarshal(coerced, &args); err != nil {
		t.Fatal(err)
	}
	if args.TimeoutMS != 250 || !args.ReplaceAll {
		t.Fatalf("unexpected coerced args: %+v", args)
	}
}

func TestValidateToolArgumentsPathAwareErrors(t *testing.T) {
	schema := json.RawMessage(`{
		"type":"object",
		"properties":{
			"edits":{"type":"array","items":{"type":"object","properties":{"old_string":{"type":"string"}},"required":["old_string"],"additionalProperties":false}}
		},
		"required":["edits"],
		"additionalProperties":false
	}`)

	err := ValidateToolArguments(schema, json.RawMessage(`{"edits":[{}]}`))
	if err == nil {
		t.Fatal("expected validation error")
	}
	if !strings.Contains(err.Error(), `edits[0].old_string`) {
		t.Fatalf("expected path-aware error, got %v", err)
	}
}

func TestValidateToolArgumentsArrayValidation(t *testing.T) {
	schema := json.RawMessage(`{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":3}`)

	if err := ValidateToolArguments(schema, json.RawMessage(`[1]`)); err == nil || !strings.Contains(err.Error(), `root`) {
		t.Fatalf("expected minItems error, got %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`[1,2,3,4]`)); err == nil || !strings.Contains(err.Error(), `root`) {
		t.Fatalf("expected maxItems error, got %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`[1,"2"]`)); err != nil {
		t.Fatalf("string integer should coerce: %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`[1,"two"]`)); err == nil || !strings.Contains(err.Error(), `[1]`) {
		t.Fatalf("expected item type error, got %v", err)
	}
}

func TestValidateToolArgumentsEnumValidation(t *testing.T) {
	schema := json.RawMessage(`{"type":"object","properties":{"mode":{"type":"string","enum":["content","count"]}},"required":["mode"]}`)

	if err := ValidateToolArguments(schema, json.RawMessage(`{"mode":"files"}`)); err == nil || !strings.Contains(err.Error(), `"mode"`) {
		t.Fatalf("expected enum error, got %v", err)
	}
	if err := ValidateToolArguments(schema, json.RawMessage(`{"mode":"content"}`)); err != nil {
		t.Fatalf("expected enum value to pass: %v", err)
	}
}
