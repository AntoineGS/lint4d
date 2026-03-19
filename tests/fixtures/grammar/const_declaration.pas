unit ConstDeclaration;

interface

const
  MAX_SIZE = 100;
  DEFAULT_NAME = 'untitled';
  PI_APPROX = 3.14159;

type
  TColor = (clRed, clGreen, clBlue);

const
  TypedConst: Integer = 42;

implementation

procedure TestLocalConst;
const
  LOCAL_LIMIT = 50;
begin
  WriteLn(LOCAL_LIMIT);
end;

end.
