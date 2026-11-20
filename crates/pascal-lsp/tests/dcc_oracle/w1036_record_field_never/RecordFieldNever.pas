unit RecordFieldNever;

interface

type
  TBox = record
    Value: Integer;
  end;

implementation

procedure Demo;
var
  Box: TBox;
begin
  WriteLn(Box.Value);
  Box.Value := 1;
  WriteLn(Box.Value);
end;

end.
