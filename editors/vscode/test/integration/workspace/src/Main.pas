unit Main;

interface

procedure Run;

implementation

uses
  Helpers;

type
  TPoint2 = record
    X, Y: Integer;
  end;

procedure Run;
var
  Point: TPoint2;
begin
  with Point do
    X := Twice(Y);
end;

end.
