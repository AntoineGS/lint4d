unit StrictPrivate;

interface

type
  TFoo = class
  strict private
    FStrictUnused: Integer;
    procedure StrictUnused;
  end;

implementation

procedure TFoo.StrictUnused;
begin
end;

end.
