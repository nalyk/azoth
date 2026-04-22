// Fixture package for PR 2.1-D Go tree-sitter extraction tests.
//
// Shape-complete: exercises every node kind the classifier in
// `code_graph::go` emits — package_clause, function_declaration,
// method_declaration (value + pointer receivers), type_spec
// (struct + interface), type_alias (Go 1.9+ `type X = Y`),
// method_elem (interface members), const_spec (single + block +
// multi-name).
//
// 500+ LOC so the `perf_budget_500_loc` test exercises the walker
// on realistic-sized input rather than a toy tree.

package sample

import (
	"fmt"
	"io"
	"strings"
)

// -----------------------------------------------------------------
// Const declarations (single + block + multi-name).
// -----------------------------------------------------------------

const Alpha = 1

const (
	Beta    = 2
	Gamma   = 3
	Delta   = 4
	Epsilon = 5
)

const (
	Pi  = 3.14159
	Tau = 6.28318
)

// Multi-name const in a single spec.
const One, Two, Three = 1, 2, 3

// -----------------------------------------------------------------
// Basic types.
// -----------------------------------------------------------------

// WidgetId is a distinct integer type.
type WidgetId int

// UserId is a type alias, not a new type — exercises the
// `type_alias` node kind (Go 1.9+ alias syntax).
type UserId = int

// StringMap is another alias for a more complex type.
type StringMap = map[string]string

// -----------------------------------------------------------------
// Structs.
// -----------------------------------------------------------------

type Widget struct {
	Id    WidgetId
	Name  string
	value int
}

type Renderer struct {
	prefix string
	suffix string
}

type Config struct {
	MaxItems int
	Timeout  int
	Debug    bool
}

// -----------------------------------------------------------------
// Interfaces — `type_spec` with `interface_type` child; members are
// `method_elem` nodes (NOT method_declaration).
// -----------------------------------------------------------------

type Drawable interface {
	Draw() string
	Bounds() (int, int)
}

type Renderable interface {
	Render(io.Writer) error
	Name() string
}

type Identifier interface {
	Id() WidgetId
}

// Embedded interface — `interface_type` contains a type reference
// alongside method_elem entries. Our classifier still sees the
// method_elem nodes under the composed interface type.
type Full interface {
	Drawable
	Renderable
	Identifier
}

// -----------------------------------------------------------------
// Top-level functions.
// -----------------------------------------------------------------

func TopFunction(x int) int {
	return x + 1
}

func Sum(xs []int) int {
	total := 0
	for _, v := range xs {
		total += v
	}
	return total
}

func Max(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func FormatWidget(w Widget) string {
	return fmt.Sprintf("Widget(id=%d, name=%q)", w.Id, w.Name)
}

func Parse(input string) (Widget, error) {
	parts := strings.Split(input, ":")
	if len(parts) != 2 {
		return Widget{}, fmt.Errorf("bad input: %q", input)
	}
	return Widget{Name: parts[0]}, nil
}

func Reverse(xs []int) []int {
	out := make([]int, len(xs))
	for i, v := range xs {
		out[len(xs)-1-i] = v
	}
	return out
}

func anotherFn() {}

func helperOne() int { return 1 }

func helperTwo() int { return 2 }

func helperThree() int { return 3 }

// -----------------------------------------------------------------
// Methods on Widget (pointer + value receivers).
// -----------------------------------------------------------------

func (w *Widget) GetValue() int {
	return w.value
}

func (w *Widget) SetValue(v int) {
	w.value = v
}

func (w Widget) String() string {
	return fmt.Sprintf("Widget(%d)", w.value)
}

func (w Widget) Draw() string {
	return "<widget/>"
}

func (w Widget) Bounds() (int, int) {
	return 100, 50
}

func (w Widget) Render(out io.Writer) error {
	_, err := fmt.Fprintf(out, "%s\n", w.String())
	return err
}

func (w Widget) Name() string {
	return w.Name
}

func (w Widget) Id() WidgetId {
	return w.Id
}

// -----------------------------------------------------------------
// Methods on Renderer.
// -----------------------------------------------------------------

func (r *Renderer) Prefix() string {
	return r.prefix
}

func (r *Renderer) Suffix() string {
	return r.suffix
}

func (r *Renderer) Wrap(body string) string {
	var sb strings.Builder
	sb.WriteString(r.prefix)
	sb.WriteString(body)
	sb.WriteString(r.suffix)
	return sb.String()
}

func (r Renderer) Copy() Renderer {
	return Renderer{prefix: r.prefix, suffix: r.suffix}
}

// -----------------------------------------------------------------
// Methods on Config.
// -----------------------------------------------------------------

func (c *Config) Validate() error {
	if c.MaxItems < 0 {
		return fmt.Errorf("max_items must be non-negative")
	}
	if c.Timeout < 0 {
		return fmt.Errorf("timeout must be non-negative")
	}
	return nil
}

func (c Config) IsDefault() bool {
	return c.MaxItems == 0 && c.Timeout == 0 && !c.Debug
}

func (c Config) Describe() string {
	return fmt.Sprintf("Config(max=%d, timeout=%d, debug=%t)", c.MaxItems, c.Timeout, c.Debug)
}

// -----------------------------------------------------------------
// Test-style functions (mirroring _test.go naming convention;
// extractor must treat them as plain functions, no special case).
// -----------------------------------------------------------------

func TestTopFunction() {
	if TopFunction(1) != 2 {
		panic("TopFunction broken")
	}
}

func TestSum() {
	if Sum([]int{1, 2, 3}) != 6 {
		panic("Sum broken")
	}
}

func TestMax() {
	if Max(1, 2) != 2 {
		panic("Max broken")
	}
}

func TestReverse() {
	got := Reverse([]int{1, 2, 3})
	if got[0] != 3 || got[2] != 1 {
		panic("Reverse broken")
	}
}

func BenchmarkSum() {
	for i := 0; i < 100; i++ {
		_ = Sum([]int{i, i + 1, i + 2})
	}
}

// -----------------------------------------------------------------
// Additional helpers to pad LOC past 500.
// -----------------------------------------------------------------

func pad01() int { return 1 }
func pad02() int { return 2 }
func pad03() int { return 3 }
func pad04() int { return 4 }
func pad05() int { return 5 }
func pad06() int { return 6 }
func pad07() int { return 7 }
func pad08() int { return 8 }
func pad09() int { return 9 }
func pad10() int { return 10 }
func pad11() int { return 11 }
func pad12() int { return 12 }
func pad13() int { return 13 }
func pad14() int { return 14 }
func pad15() int { return 15 }
func pad16() int { return 16 }
func pad17() int { return 17 }
func pad18() int { return 18 }
func pad19() int { return 19 }
func pad20() int { return 20 }

type padStruct01 struct{ f int }
type padStruct02 struct{ f int }
type padStruct03 struct{ f int }
type padStruct04 struct{ f int }
type padStruct05 struct{ f int }

func (p *padStruct01) Value() int { return p.f }
func (p *padStruct02) Value() int { return p.f }
func (p *padStruct03) Value() int { return p.f }
func (p *padStruct04) Value() int { return p.f }
func (p *padStruct05) Value() int { return p.f }

const (
	padConst01 = 1
	padConst02 = 2
	padConst03 = 3
	padConst04 = 4
	padConst05 = 5
	padConst06 = 6
	padConst07 = 7
	padConst08 = 8
	padConst09 = 9
	padConst10 = 10
)

type padAlias01 = int
type padAlias02 = string
type padAlias03 = []int
type padAlias04 = map[string]int

// More realistic utility functions.

func clampInt(x, lo, hi int) int {
	if x < lo {
		return lo
	}
	if x > hi {
		return hi
	}
	return x
}

func absInt(x int) int {
	if x < 0 {
		return -x
	}
	return x
}

func signInt(x int) int {
	switch {
	case x > 0:
		return 1
	case x < 0:
		return -1
	default:
		return 0
	}
}

func repeatString(s string, n int) string {
	var sb strings.Builder
	for i := 0; i < n; i++ {
		sb.WriteString(s)
	}
	return sb.String()
}

func trimPrefix(s, prefix string) string {
	return strings.TrimPrefix(s, prefix)
}

func trimSuffix(s, suffix string) string {
	return strings.TrimSuffix(s, suffix)
}

func joinStrings(xs []string, sep string) string {
	return strings.Join(xs, sep)
}

func splitStrings(s, sep string) []string {
	return strings.Split(s, sep)
}

func containsString(s, sub string) bool {
	return strings.Contains(s, sub)
}

func lowerString(s string) string {
	return strings.ToLower(s)
}

func upperString(s string) string {
	return strings.ToUpper(s)
}

// -----------------------------------------------------------------
// Additional interfaces for coverage.
// -----------------------------------------------------------------

type Closer interface {
	Close() error
}

type Reader interface {
	Read(p []byte) (int, error)
}

type Writer interface {
	Write(p []byte) (int, error)
}

type ReadWriter interface {
	Reader
	Writer
}

type ReadWriteCloser interface {
	Reader
	Writer
	Closer
}

// -----------------------------------------------------------------
// Additional structs.
// -----------------------------------------------------------------

type Point struct {
	X, Y int
}

type Vector struct {
	X, Y, Z float64
}

type Rectangle struct {
	Min, Max Point
}

func (r Rectangle) Width() int  { return r.Max.X - r.Min.X }
func (r Rectangle) Height() int { return r.Max.Y - r.Min.Y }
func (r Rectangle) Area() int   { return r.Width() * r.Height() }

func (v Vector) Magnitude() float64 {
	return v.X*v.X + v.Y*v.Y + v.Z*v.Z
}

func (p Point) Translate(dx, dy int) Point {
	return Point{X: p.X + dx, Y: p.Y + dy}
}

// Various additional helpers to hit 500 LOC.

func anotherHelperOne(x int) int   { return x * 2 }
func anotherHelperTwo(x int) int   { return x * 3 }
func anotherHelperThree(x int) int { return x * 4 }
func anotherHelperFour(x int) int  { return x * 5 }
func anotherHelperFive(x int) int  { return x * 6 }
