unit PrivateDynamic;

interface

type
  TFoo = class
  private
    procedure UncalledDynamic; dynamic;
  end;

implementation

procedure TFoo.UncalledDynamic;
begin
end;

end.
