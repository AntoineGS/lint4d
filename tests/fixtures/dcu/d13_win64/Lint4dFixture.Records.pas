unit Lint4dFixture.Records;

interface

type
  TSimpleRecord = record
    Name: string;
    Value: Integer;
    Active: Boolean;
  end;

  TAdvancedRecord = record
  private
    FX: Double;
    FY: Double;
  public
    constructor Create(AX, AY: Double);
    function Distance: Double;
    class function Origin: TAdvancedRecord; static;
    class operator Equal(const A, B: TAdvancedRecord): Boolean;
  end;

implementation

uses
  System.Math;

{ TAdvancedRecord }

constructor TAdvancedRecord.Create(AX, AY: Double);
begin
  FX := AX;
  FY := AY;
end;

function TAdvancedRecord.Distance: Double;
begin
  Result := Sqrt(FX * FX + FY * FY);
end;

class function TAdvancedRecord.Origin: TAdvancedRecord;
begin
  Result := TAdvancedRecord.Create(0, 0);
end;

class operator TAdvancedRecord.Equal(const A, B: TAdvancedRecord): Boolean;
begin
  Result := (A.FX = B.FX) and (A.FY = B.FY);
end;

end.
