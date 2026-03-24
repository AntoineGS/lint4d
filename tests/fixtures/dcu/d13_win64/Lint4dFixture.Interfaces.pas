unit Lint4dFixture.Interfaces;

interface

uses
  Lint4dFixture.Classes;

type
  ISimpleInterface = interface
    ['{A1B2C3D4-E5F6-7890-ABCD-EF1234567890}']
    function GetValue: Integer;
    procedure SetValue(AValue: Integer);
  end;

  IComplexInterface = interface(ISimpleInterface)
    ['{B2C3D4E5-F6A7-8901-BCDE-F12345678901}']
    function GetName: string;
    procedure Execute;
  end;

  TInterfacedImpl = class(TInterfacedObject, ISimpleInterface)
  private
    FInner: TSimpleClass;
    FValue: Integer;
  public
    constructor Create;
    destructor Destroy; override;
    function GetValue: Integer;
    procedure SetValue(AValue: Integer);
  end;

implementation

{ TInterfacedImpl }

constructor TInterfacedImpl.Create;
begin
  inherited;
  FInner := TSimpleClass.Create('Inner', 0);
  FValue := 0;
end;

destructor TInterfacedImpl.Destroy;
begin
  FInner.Free;
  inherited;
end;

function TInterfacedImpl.GetValue: Integer;
begin
  Result := FValue;
end;

procedure TInterfacedImpl.SetValue(AValue: Integer);
begin
  FValue := AValue;
end;

end.
