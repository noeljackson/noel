package tools

import "encoding/json"

type BashDetails struct {
	ExitCode    int    `json:"exitCode"`
	StdoutBytes int    `json:"stdoutBytes"`
	StderrBytes int    `json:"stderrBytes"`
	Command     string `json:"command"`
	DurationMS  int    `json:"durationMS"`
	OutputFile  string `json:"outputFile,omitempty"`
}

type ReadDetails struct {
	Path      string `json:"path"`
	Lines     int    `json:"lines"`
	Bytes     int    `json:"bytes"`
	Truncated bool   `json:"truncated"`
	StartLine int    `json:"startLine,omitempty"`
}

type WriteDetails struct {
	Path  string `json:"path"`
	Bytes int    `json:"bytes"`
	Lines int    `json:"lines"`
}

type EditDetails struct {
	Path         string     `json:"path"`
	EditsApplied int        `json:"editsApplied"`
	BeforeLines  int        `json:"beforeLines"`
	AfterLines   int        `json:"afterLines"`
	Hunks        []EditHunk `json:"hunks,omitempty"`
}

type EditHunk struct {
	OldText string `json:"oldText,omitempty"`
	NewText string `json:"newText,omitempty"`
}

type GrepDetails struct {
	Pattern    string `json:"pattern"`
	Files      int    `json:"files"`
	Matches    int    `json:"matches"`
	OutputMode string `json:"outputMode"`
}

type FindDetails struct {
	Pattern string `json:"pattern"`
	Hits    int    `json:"hits"`
	Limit   int    `json:"limit"`
}

type LsDetails struct {
	Path    string `json:"path"`
	Entries int    `json:"entries"`
	Limit   int    `json:"limit"`
}

func MarshalDetails(details interface{}) (json.RawMessage, error) {
	if details == nil {
		return nil, nil
	}
	encoded, err := json.Marshal(details)
	if err != nil {
		return nil, err
	}
	return encoded, nil
}
