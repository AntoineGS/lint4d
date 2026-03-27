unit RecordHelper;

interface

type
  TIntHelper = record helper for Integer
    function ToString: string;
    function IsPositive: Boolean;
  end;

  TMyRecord = record
    X: Integer;
    Y: Integer;
    class operator Add(const A, B: TMyRecord): TMyRecord;
  end;

implementation

function TIntHelper.ToString: string;
begin
  Result := IntToStr(Self);
end;

function TIntHelper.IsPositive: Boolean;
begin
  Result := Self > 0;
end;

class operator TMyRecord.Add(const A, B: TMyRecord): TMyRecord;
begin
  Result.X := A.X + B.X;
  Result.Y := A.Y + B.Y;
end;

end.
