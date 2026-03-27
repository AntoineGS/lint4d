unit Lint4dFixture.Classes;

interface

uses
  SysUtils,
  Classes;

type
  TSimpleClass = class
  private
    FName: string;
    FValue: Integer;
    FOwner: TObject;
  public
    constructor Create(const AName: string; AValue: Integer);
    destructor Destroy; override;
    function GetName: string;
    procedure SetValue(AValue: Integer);
  end;

  TFieldHeavyClass = class
  private
    FStr: string;
    FInt: Integer;
    FBool: Boolean;
    FDouble: Double;
    FDateTime: TDateTime;
    FObj: TObject;
    FArray: TArray<Integer>;
    FInt64Val: Int64;
    FByte: Byte;
    FWord: Word;
    FCardinal: Cardinal;
    FCurrency: Currency;
    FWideStr: WideString;
    FAnsiStr: AnsiString;
    FChar: Char;
    FPointer: Pointer;
  public
    constructor Create;
    destructor Destroy; override;
  end;

  TDataAdapter = class
  private
    FDatabase: TObject;
    FCache: TObject;
    FObject: TObject;
    function GetAllowedFields(AUsageId: Integer): TArray<string>;
    function RequeryRow(const ASQL: string): string;
  public
    constructor Create(ADatabase: TObject; ACache: TObject);
    destructor Destroy; override;
    procedure Execute(const ACommand: string);
    function Query(const ASQL: string): string;
  end;

implementation

{ TSimpleClass }

constructor TSimpleClass.Create(const AName: string; AValue: Integer);
begin
  inherited Create;
  FName := AName;
  FValue := AValue;
  FOwner := nil;
end;

destructor TSimpleClass.Destroy;
begin
  FOwner := nil;
  inherited;
end;

function TSimpleClass.GetName: string;
begin
  Result := FName;
end;

procedure TSimpleClass.SetValue(AValue: Integer);
begin
  FValue := AValue;
end;

{ TFieldHeavyClass }

constructor TFieldHeavyClass.Create;
begin
  inherited;
  FObj := nil;
end;

destructor TFieldHeavyClass.Destroy;
begin
  FObj.Free;
  inherited;
end;

{ TDataAdapter }

constructor TDataAdapter.Create(ADatabase: TObject; ACache: TObject);
begin
  inherited Create;
  FDatabase := ADatabase;
  FCache := ACache;
  FObject := TObject.Create;
end;

destructor TDataAdapter.Destroy;
begin
  FObject.Free;
  inherited;
end;

function TDataAdapter.GetAllowedFields(AUsageId: Integer): TArray<string>;
begin
  Result := nil;
end;

function TDataAdapter.RequeryRow(const ASQL: string): string;
begin
  Result := '';
end;

procedure TDataAdapter.Execute(const ACommand: string);
begin
  // stub
end;

function TDataAdapter.Query(const ASQL: string): string;
begin
  Result := RequeryRow(ASQL);
end;

end.
