unit W1000ConstAndVar;

interface

const
  DeprecatedConst = 7 deprecated;

var
  DeprecatedVar: Integer deprecated;

procedure UseDeprecatedValues;

implementation

procedure UseDeprecatedValues;
var
  Value: Integer;
begin
  Value := DeprecatedConst;
  Value := DeprecatedVar;
end;

end.
