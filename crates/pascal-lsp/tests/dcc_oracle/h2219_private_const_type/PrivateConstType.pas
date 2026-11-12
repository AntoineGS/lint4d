unit PrivateConstType;

interface

type
  TFoo = class
  private
    const CUnused = 1;
    type TUnused = Integer;
  end;

implementation

end.
