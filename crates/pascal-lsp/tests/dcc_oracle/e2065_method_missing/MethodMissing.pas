unit MethodMissing;

interface

type
  TFoo = class
    procedure Missing;
    procedure Present;
  end;

implementation

procedure TFoo.Present;
begin
end;

end.
