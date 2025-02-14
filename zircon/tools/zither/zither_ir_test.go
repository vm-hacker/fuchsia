// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package zither_test

import (
	"fmt"
	"sort"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
	"go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgentest"
	"go.fuchsia.dev/fuchsia/zircon/tools/zither"
)

// Permits the comparison of unexported members of fidlgen.{Library,}Name.
var cmpNameOpt = cmp.AllowUnexported(fidlgen.LibraryName{}, fidlgen.Name{})

func TestGeneratedFileCount(t *testing.T) {
	{
		ir := fidlgentest.EndToEndTest{T: t}.Single(`
	library example;

	const A bool = true;
	`)

		summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
		if err != nil {
			t.Fatal(err)
		}
		if len(summaries) != 1 {
			t.Fatalf("expected one summary; got %d", len(summaries))
		}
	}

	{
		ir := fidlgentest.EndToEndTest{T: t}.Multiple([]string{
			`
	library example;

	const A bool = true;
	`,
			`
	library example;

	const B bool = true;
	`,
			`
	library example;

	const C bool = true;
	`,
		})

		summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
		if err != nil {
			t.Fatal(err)
		}
		if len(summaries) != 3 {
			t.Fatalf("expected three summaries; got %d", len(summaries))
		}
	}
}

func TestCanSummarizeLibraryName(t *testing.T) {
	name := "this.is.an.example.library"
	ir := fidlgentest.EndToEndTest{T: t}.Single(fmt.Sprintf(`
	library %s;

	const A bool = true;
	`, name))

	summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
	if err != nil {
		t.Fatal(err)
	}
	if summaries[0].Library.String() != name {
		t.Errorf("expected %s; got %s", name, summaries[0].Library)
	}
}

func TestDeclOrder(t *testing.T) {
	ir := fidlgentest.EndToEndTest{T: t}.Single(`
library example;

const A int32 = 0;
const B int32 = E;
const C int32 = A;
const D int32 = 1;
const E int32 = C;
const F int32 = B;
const G int32 = 2;
`)

	{
		summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
		if err != nil {
			t.Fatal(err)
		}

		var actual []string
		for _, decl := range summaries[0].Decls {
			actual = append(actual, decl.Name().String())
		}
		expected := []string{
			"example/A",
			"example/B",
			"example/C",
			"example/D",
			"example/E",
			"example/F",
			"example/G",
		}
		if diff := cmp.Diff(expected, actual); diff != "" {
			t.Error(diff)
		}
	}

	{
		summaries, err := zither.Summarize(ir, zither.DependencyDeclOrder)
		if err != nil {
			t.Fatal(err)
		}

		var actual []string
		for _, decl := range summaries[0].Decls {
			actual = append(actual, decl.Name().String())
		}
		expected := []string{
			"example/A",
			"example/C",
			"example/D", // C and D have no interdependencies, and D follows C in source.
			"example/E",
			"example/B",
			"example/F",
			"example/G",
		}
		if diff := cmp.Diff(expected, actual); diff != "" {
			t.Error(diff)
		}
	}
}

func TestFloatConstantsAreDisallowed(t *testing.T) {
	decls := []string{
		"const FLOAT32 float32 = 0.0;",
		"const FLOAT64 float64 = 0.0;",
	}

	for _, decl := range decls {
		ir := fidlgentest.EndToEndTest{T: t}.Single(fmt.Sprintf(`
library example;

%s
`, decl))

		_, err := zither.Summarize(ir, zither.SourceDeclOrder)
		if err == nil {
			t.Fatal("expected an error")
		}
		if err.Error() != "floats are unsupported" {
			t.Errorf("unexpected error: %v", err)
		}
	}
}

func TestCanSummarizeConstants(t *testing.T) {
	ir := fidlgentest.EndToEndTest{T: t}.Single(`
library example;

const BOOL bool = false;

const BINARY_UINT8 uint8 = 0b10101111;

const HEX_UINT16 uint16 = 0xabcd;

const DECIMAL_UINT32 uint32 = 123456789;

const BINARY_INT8 int8 = 0b1111010;

const HEX_INT16 int16 = 0xcba;

const NEGATIVE_HEX_INT16 int16 = -0xcba;

const DECIMAL_INT32 int32 = 1050065;

const NEGATIVE_DECIMAL_INT32 int32 = -1050065;

const UINT64_MAX uint64 = 0xffffffffffffffff;

const INT64_MIN int64 = -0x8000000000000000;

const SOME_STRING string = "XXX";

const DEFINED_IN_TERMS_OF_ANOTHER_STRING string = SOME_STRING;

const DEFINED_IN_TERMS_OF_ANOTHER_UINT16 uint16 = HEX_UINT16;

type Uint8Enum = strict enum : uint8 {
	MAX = 0xff;
};

const UINT8_ENUM_VALUE Uint8Enum = Uint8Enum.MAX;

/// This is a one-line comment.
const COMMENTED_BOOL bool = true;

/// This is
///   a
///       many-line
/// comment.
const COMMENTED_STRING string = "YYY";
`)
	summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
	if err != nil {
		t.Fatal(err)
	}

	var actual []zither.Const
	for _, decl := range summaries[0].Decls {
		if decl.IsConst() {
			actual = append(actual, decl.AsConst())
		}
	}

	someStringName := fidlgen.MustReadName("example/SOME_STRING")
	hexUint16Name := fidlgen.MustReadName("example/HEX_UINT16")
	uint8EnumMaxName := fidlgen.MustReadName("example/Uint8Enum.MAX")

	// Listed in declaration order for readability, but similarly sorted.
	expected := []zither.Const{
		{
			Name:  fidlgen.MustReadName("example/BOOL"),
			Kind:  zither.TypeKindBool,
			Type:  "bool",
			Value: "false",
		},
		{
			Name:  fidlgen.MustReadName("example/BINARY_UINT8"),
			Kind:  zither.TypeKindInteger,
			Type:  "uint8",
			Value: "0b10101111",
		},
		{
			Name:  fidlgen.MustReadName("example/HEX_UINT16"),
			Kind:  zither.TypeKindInteger,
			Type:  "uint16",
			Value: "0xabcd",
		},
		{
			Name:  fidlgen.MustReadName("example/DECIMAL_UINT32"),
			Kind:  zither.TypeKindInteger,
			Type:  "uint32",
			Value: "123456789",
		},
		{
			Name:  fidlgen.MustReadName("example/BINARY_INT8"),
			Kind:  zither.TypeKindInteger,
			Type:  "int8",
			Value: "0b1111010",
		},
		{
			Name:  fidlgen.MustReadName("example/HEX_INT16"),
			Kind:  zither.TypeKindInteger,
			Type:  "int16",
			Value: "0xcba",
		},
		{
			Name:  fidlgen.MustReadName("example/NEGATIVE_HEX_INT16"),
			Kind:  zither.TypeKindInteger,
			Type:  "int16",
			Value: "-0xcba",
		},
		{
			Name:  fidlgen.MustReadName("example/DECIMAL_INT32"),
			Kind:  zither.TypeKindInteger,
			Type:  "int32",
			Value: "1050065",
		},
		{
			Name:  fidlgen.MustReadName("example/NEGATIVE_DECIMAL_INT32"),
			Kind:  zither.TypeKindInteger,
			Type:  "int32",
			Value: "-1050065",
		},
		{
			Name:  fidlgen.MustReadName("example/UINT64_MAX"),
			Kind:  zither.TypeKindInteger,
			Type:  "uint64",
			Value: "0xffffffffffffffff",
		},
		{
			Name:  fidlgen.MustReadName("example/INT64_MIN"),
			Kind:  zither.TypeKindInteger,
			Type:  "int64",
			Value: "-0x8000000000000000",
		},
		{
			Name:  fidlgen.MustReadName("example/SOME_STRING"),
			Kind:  zither.TypeKindString,
			Type:  "string",
			Value: "XXX",
		},
		{
			Name:       fidlgen.MustReadName("example/DEFINED_IN_TERMS_OF_ANOTHER_STRING"),
			Kind:       zither.TypeKindString,
			Type:       "string",
			Value:      "XXX",
			Identifier: &someStringName,
		},
		{
			Name:       fidlgen.MustReadName("example/DEFINED_IN_TERMS_OF_ANOTHER_UINT16"),
			Kind:       zither.TypeKindInteger,
			Type:       "uint16",
			Value:      "43981",
			Identifier: &hexUint16Name,
		},
		{
			Name:       fidlgen.MustReadName("example/UINT8_ENUM_VALUE"),
			Kind:       zither.TypeKindEnum,
			Type:       "example/Uint8Enum",
			Value:      "255",
			Identifier: &uint8EnumMaxName,
		},
		{
			Name:     fidlgen.MustReadName("example/COMMENTED_BOOL"),
			Kind:     zither.TypeKindBool,
			Type:     "bool",
			Value:    "true",
			Comments: []string{" This is a one-line comment."},
		},
		{
			Name:     fidlgen.MustReadName("example/COMMENTED_STRING"),
			Kind:     zither.TypeKindString,
			Type:     "string",
			Value:    "YYY",
			Comments: []string{" This is", "   a", "       many-line", " comment."},
		},
	}

	if diff := cmp.Diff(expected, actual, cmpNameOpt); diff != "" {
		t.Error(diff)
	}
}

func TestCanSummarizeEnums(t *testing.T) {
	ir := fidlgentest.EndToEndTest{T: t}.Single(`
library example;

/// This is a uint8 enum.
type Uint8Enum = enum : uint8 {
  /// This is a member.
  TWO = 0b10;

  /// This is
  /// another
  /// member.
  SEVENTEEN = 17;
};

/// This
/// is
/// an
/// int64 enum.
type Int64Enum = enum : int64 {
  MINUS_HEX_ABCD = -0xabcd;
  HEX_DEADBEEF = 0xdeadbeef;
};
`)
	summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
	if err != nil {
		t.Fatal(err)
	}

	// Normalize member order by name for a stable comparison.
	var actual []zither.Enum
	for _, decl := range summaries[0].Decls {
		if decl.IsEnum() {
			enum := decl.AsEnum()
			sort.Slice(enum.Members, func(i, j int) bool {
				return strings.Compare(enum.Members[i].Name, enum.Members[j].Name) < 0
			})
			actual = append(actual, enum)
		}
	}

	expected := []zither.Enum{
		{
			Subtype:  "uint8",
			Name:     fidlgen.MustReadName("example/Uint8Enum"),
			Comments: []string{" This is a uint8 enum."},
			Members: []zither.EnumMember{
				{
					Name:     "SEVENTEEN",
					Value:    "17",
					Comments: []string{" This is", " another", " member."},
				},
				{
					Name:     "TWO",
					Value:    "0b10",
					Comments: []string{" This is a member."},
				},
			},
		},
		{
			Subtype:  "int64",
			Name:     fidlgen.MustReadName("example/Int64Enum"),
			Comments: []string{" This", " is", " an", " int64 enum."},
			Members: []zither.EnumMember{
				{
					Name:  "HEX_DEADBEEF",
					Value: "0xdeadbeef",
				},
				{
					Name:  "MINUS_HEX_ABCD",
					Value: "-0xabcd",
				},
			},
		},
	}

	if diff := cmp.Diff(expected, actual, cmpNameOpt); diff != "" {
		t.Error(diff)
	}
}

func TestCanSummarizeBits(t *testing.T) {
	ir := fidlgentest.EndToEndTest{T: t}.Single(`
library example;

/// This is a uint8 bits.
type Uint8Bits = bits : uint8 {
  /// This is a member.
  ONE = 0b1;

  /// This is
  /// another
  /// member.
  SIXTEEN = 16;
};

/// This
/// is
/// a
/// uint64 bits.
type Uint64Bits = bits : uint64 {
  MEMBER = 0x1000;
};
`)
	summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
	if err != nil {
		t.Fatal(err)
	}

	// Normalize member order by name for a stable comparison.
	var actual []zither.Bits
	for _, decl := range summaries[0].Decls {
		if decl.IsBits() {
			bits := decl.AsBits()
			sort.Slice(bits.Members, func(i, j int) bool {
				return strings.Compare(bits.Members[i].Name, bits.Members[j].Name) < 0
			})
			actual = append(actual, bits)
		}
	}

	expected := []zither.Bits{
		{
			Subtype:  fidlgen.Uint8,
			Name:     fidlgen.MustReadName("example/Uint8Bits"),
			Comments: []string{" This is a uint8 bits."},
			Members: []zither.BitsMember{
				{
					Name:     "ONE",
					Index:    0,
					Comments: []string{" This is a member."},
				},
				{
					Name:     "SIXTEEN",
					Index:    4,
					Comments: []string{" This is", " another", " member."},
				},
			},
		},
		{
			Subtype:  fidlgen.Uint64,
			Name:     fidlgen.MustReadName("example/Uint64Bits"),
			Comments: []string{" This", " is", " a", " uint64 bits."},
			Members: []zither.BitsMember{
				{
					Name:  "MEMBER",
					Index: 12,
				},
			},
		},
	}

	if diff := cmp.Diff(expected, actual, cmpNameOpt); diff != "" {
		t.Error(diff)
	}
}

func TestCanSummarizeStructs(t *testing.T) {
	ir := fidlgentest.EndToEndTest{T: t}.Single(`
library example;

/// This is a struct.
type EmptyStruct = struct {};

type BasicStruct = struct {
	/// This is a struct member.
    i64 int64;
    u64 uint64;
    i32 int32;
    u32 uint32;
    i16 int16;
    u16 uint16;
    i8 int8;
    u8 uint8;
    b bool;
	e Enum;
	bits Bits;
	empty EmptyStruct;
};

type Enum = enum : uint16 {
	ZERO = 0;
};

type Bits = bits : uint16 {
	ONE = 1;
};

type StructWithArrayMembers = struct {
    u8s array<uint8, 10>;
    empties array<EmptyStruct, 6>;
    nested array<array<bool, 2>, 4>;
};
`)
	summaries, err := zither.Summarize(ir, zither.SourceDeclOrder)
	if err != nil {
		t.Fatal(err)
	}

	var actual []zither.Struct
	for _, decl := range summaries[0].Decls {
		if decl.IsStruct() {
			actual = append(actual, decl.AsStruct())
		}
	}

	// Addressable integers for use as TypeDescriptor.ElementCount below.
	two, four, six, ten := 2, 4, 6, 10

	expected := []zither.Struct{
		{
			Name:     fidlgen.MustReadName("example/EmptyStruct"),
			Comments: []string{" This is a struct."},
		},
		{
			Name: fidlgen.MustReadName("example/BasicStruct"),
			Members: []zither.StructMember{
				{
					Name: "i64",
					Type: zither.TypeDescriptor{
						Type: "int64",
						Kind: zither.TypeKindInteger,
					},
					Comments: []string{" This is a struct member."},
				},
				{
					Name: "u64",
					Type: zither.TypeDescriptor{
						Type: "uint64",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "i32",
					Type: zither.TypeDescriptor{
						Type: "int32",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "u32",
					Type: zither.TypeDescriptor{
						Type: "uint32",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "i16",
					Type: zither.TypeDescriptor{
						Type: "int16",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "u16",
					Type: zither.TypeDescriptor{
						Type: "uint16",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "i8",
					Type: zither.TypeDescriptor{
						Type: "int8",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "u8",
					Type: zither.TypeDescriptor{
						Type: "uint8",
						Kind: zither.TypeKindInteger,
					},
				},
				{
					Name: "b",
					Type: zither.TypeDescriptor{
						Type: "bool",
						Kind: zither.TypeKindBool,
					},
				},
				{
					Name: "e",
					Type: zither.TypeDescriptor{
						Type: "example/Enum",
						Kind: zither.TypeKindEnum,
					},
				},
				{
					Name: "bits",
					Type: zither.TypeDescriptor{
						Type: "example/Bits",
						Kind: zither.TypeKindBits,
					},
				},
				{
					Name: "empty",
					Type: zither.TypeDescriptor{
						Type: "example/EmptyStruct",
						Kind: zither.TypeKindStruct,
					},
				},
			},
		},
		{
			Name: fidlgen.MustReadName("example/StructWithArrayMembers"),
			Members: []zither.StructMember{
				{
					Name: "u8s",
					Type: zither.TypeDescriptor{
						Kind: zither.TypeKindArray,
						ElementType: &zither.TypeDescriptor{
							Type: "uint8",
							Kind: zither.TypeKindInteger,
						},
						ElementCount: &ten,
					},
				},
				{
					Name: "empties",
					Type: zither.TypeDescriptor{
						Kind: zither.TypeKindArray,
						ElementType: &zither.TypeDescriptor{
							Type: "example/EmptyStruct",
							Kind: zither.TypeKindStruct,
						},
						ElementCount: &six,
					},
				},
				{
					Name: "nested",
					Type: zither.TypeDescriptor{
						Kind: zither.TypeKindArray,
						ElementType: &zither.TypeDescriptor{
							Kind: zither.TypeKindArray,
							ElementType: &zither.TypeDescriptor{
								Type: "bool",
								Kind: zither.TypeKindBool,
							},
							ElementCount: &two,
						},
						ElementCount: &four,
					},
				},
			},
		},
	}

	if diff := cmp.Diff(expected, actual, cmpNameOpt); diff != "" {
		t.Error(diff)
	}
}
