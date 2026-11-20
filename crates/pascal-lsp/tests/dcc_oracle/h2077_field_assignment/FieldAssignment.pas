unit FieldAssignment;

interface

type
  TRecord = record
    Value: Integer;
  end;

implementation

procedure Demo;
var
  R: TRecord;
begin
  R.Value := 1;
  R.Value := 2;
  WriteLn(R.Value);
end;

end.
