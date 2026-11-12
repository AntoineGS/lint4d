unit RecursiveOnly;

interface

type
  TFoo = class
  private
    FUnused: Integer;
    procedure Recurse;
  end;

implementation

procedure TFoo.Recurse;
begin
  Recurse;
end;

end.
