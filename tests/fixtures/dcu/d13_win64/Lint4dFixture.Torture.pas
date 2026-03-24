unit Lint4dFixture.Torture;

interface

uses
  System.SysUtils,
  System.Variants,
  Lint4dFixture.Classes,
  Lint4dFixture.Interfaces,
  Lint4dFixture.Enums;

type
  TMegaClass = class
  public type
    TNestedRecord = record
      X: Integer;
      Y: Integer;
    end;
    TNestedEnum = (neFirst, neSecond, neThird);
  private
    FField01: string;
    FField02: Integer;
    FField03: Boolean;
    FField04: Double;
    FField05: Int64;
    FField06: Byte;
    FField07: Word;
    FField08: Cardinal;
    FField09: Currency;
    FField10: TDateTime;
    FField11: WideString;
    FField12: AnsiString;
    FField13: Char;
    FField14: Pointer;
    FField15: TObject;
    FField16: TSimpleClass;
    FField17: TColor;
    FField18: TColors;
    FField19: TArray<Integer>;
    FField20: TArray<string>;
    FField21: Variant;
    FField22: OleVariant;
    FField23: ShortString;
    FField24: Single;
    FNested: TNestedRecord;
    function GetItem(Index: Integer): string;
    procedure SetItem(Index: Integer; const Value: string);
    function GetName: string;
    procedure SetName(const Value: string);
  public
    class var InstanceCount: Integer;
    class function CreateDefault: TMegaClass;
    class procedure ResetCount;
    constructor Create;
    destructor Destroy; override;
    procedure Overloaded; overload;
    procedure Overloaded(A: Integer); overload;
    procedure Overloaded(A: Integer; B: string); overload;
    procedure Overloaded(A: Integer; B: string; C: Boolean); overload;
    procedure ManyParams(A1: Integer; A2: string; A3: Boolean;
      A4: Double; A5: TObject; A6: Int64; A7: Byte;
      A8: Word; A9: Cardinal; A10: Currency);
    property Items[Index: Integer]: string read GetItem write SetItem; default;
    property Name: string read GetName write SetName;
  end;

  TIndexedContainer = class
  private
    FData: TArray<Variant>;
    function GetByIndex(I: Integer): Variant;
    procedure SetByIndex(I: Integer; const V: Variant);
    function GetByName(const N: string): Variant;
    procedure SetByName(const N: string; const V: Variant);
  public
    property ByIndex[I: Integer]: Variant read GetByIndex write SetByIndex; default;
    property ByName[const N: string]: Variant read GetByName write SetByName;
  end;

implementation

{ TMegaClass }

class function TMegaClass.CreateDefault: TMegaClass;
begin
  Result := TMegaClass.Create;
end;

class procedure TMegaClass.ResetCount;
begin
  InstanceCount := 0;
end;

constructor TMegaClass.Create;
begin
  inherited;
  Inc(InstanceCount);
  FField15 := nil;
  FField16 := nil;
end;

destructor TMegaClass.Destroy;
begin
  Dec(InstanceCount);
  FField16.Free;
  FField15.Free;
  inherited;
end;

procedure TMegaClass.Overloaded;
begin
end;

procedure TMegaClass.Overloaded(A: Integer);
begin
end;

procedure TMegaClass.Overloaded(A: Integer; B: string);
begin
end;

procedure TMegaClass.Overloaded(A: Integer; B: string; C: Boolean);
begin
end;

procedure TMegaClass.ManyParams(A1: Integer; A2: string; A3: Boolean;
  A4: Double; A5: TObject; A6: Int64; A7: Byte;
  A8: Word; A9: Cardinal; A10: Currency);
begin
end;

function TMegaClass.GetItem(Index: Integer): string;
begin
  Result := '';
end;

procedure TMegaClass.SetItem(Index: Integer; const Value: string);
begin
end;

function TMegaClass.GetName: string;
begin
  Result := FField01;
end;

procedure TMegaClass.SetName(const Value: string);
begin
  FField01 := Value;
end;

{ TIndexedContainer }

function TIndexedContainer.GetByIndex(I: Integer): Variant;
begin
  Result := FData[I];
end;

procedure TIndexedContainer.SetByIndex(I: Integer; const V: Variant);
begin
  FData[I] := V;
end;

function TIndexedContainer.GetByName(const N: string): Variant;
begin
  Result := Unassigned;
end;

procedure TIndexedContainer.SetByName(const N: string; const V: Variant);
begin
end;

end.
